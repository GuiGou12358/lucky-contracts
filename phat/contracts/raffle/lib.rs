#![cfg_attr(not(feature = "std"), no_std, no_main)]

extern crate alloc;
extern crate core;

#[ink::contract(env = pink_extension::PinkEnvironment)]
mod lucky_raffle {

    use alloc::{string::String, string::ToString, vec::Vec};
    use ink::storage::Lazy;
    use phat_offchain_rollup::clients::ink::{Action, ContractId, InkRollupClient};
    use pink_extension::chain_extension::signing;
    use pink_extension::{error, info, ResultExt};
    use scale::{Decode, Encode};
    use sp_core::crypto::{AccountId32, Ss58AddressFormatRegistry, Ss58Codec};

    type CodeHash = [u8; 32];

    /// Message sent to provide the data
    /// response pushed in the queue by the offchain rollup and read by the Ink! smart contract
    #[derive(Encode, Decode)]
    enum ResponseMessage {
        JsResponse {
            /// hash of js script executed to get the data
            js_script_hash: CodeHash,
            /// hash of data in input of js
            input_hash: CodeHash,
            /// hash of settings of js
            settings_hash: CodeHash,
            /// response value
            output_value: Vec<u8>,
        },
        Error {
            /// hash of js script
            js_script_hash: CodeHash,
            /// input in js
            input_value: Vec<u8>,
            /// hash of settings of js
            settings_hash: CodeHash,
            /// when an error occurs
            error: Vec<u8>,
        },
    }

    #[ink(storage)]
    pub struct JsOffchainRollup {
        owner: AccountId,
        /// config to send the data to the ink! smart contract
        config: Option<Config>,
        /// Key for signing the rollup tx.
        attest_key: [u8; 32],
        /// The JS code that processes the rollup queue request
        core_js: Lazy<CoreJs>,
    }

    #[derive(Encode, Decode, Debug, Clone)]
    #[cfg_attr(
        feature = "std",
        derive(scale_info::TypeInfo, ink::storage::traits::StorageLayout)
    )]
    pub struct CoreJs {
        /// The JS code that processes the rollup queue request
        script: String,
        /// The configuration that would be passed to the core js script
        settings: String,
        /// The code hash of the core js script
        code_hash: CodeHash,
        /// The code hash of the settings in parameter of js script
        settings_hash: CodeHash,
    }

    #[derive(Encode, Decode, Debug)]
    #[cfg_attr(
        feature = "std",
        derive(scale_info::TypeInfo, ink::storage::traits::StorageLayout)
    )]
    struct Config {
        /// The RPC endpoint of the target blockchain
        rpc: String,
        pallet_id: u8,
        call_id: u8,
        /// The rollup anchor address on the target blockchain
        contract_id: ContractId,
        /// Key for sending out the rollup meta-tx. None to fallback to the wallet based auth.
        sender_key: Option<[u8; 32]>,
    }

    #[derive(Encode, Decode, Debug)]
    #[repr(u8)]
    #[cfg_attr(feature = "std", derive(scale_info::TypeInfo))]
    pub enum ContractError {
        BadOrigin,
        ClientNotConfigured,
        CoreNotConfigured,
        GraphApiNotConfigured,
        InvalidKeyLength,
        InvalidAddressLength,
        NoRequestInQueue,
        FailedToCreateClient,
        FailedToCommitTx,
        FailedToCallRollup,
        JsError(String),
        FailedToDecode,
        NbWinnersNotSet,
        NextEraUnknown,
    }

    type Result<T> = core::result::Result<T, ContractError>;

    impl From<phat_offchain_rollup::Error> for ContractError {
        fn from(error: phat_offchain_rollup::Error) -> Self {
            error!("error in the rollup: {:?}", error);
            ContractError::FailedToCallRollup
        }
    }

    impl JsOffchainRollup {
        #[ink(constructor)]
        pub fn default() -> Self {
            const NONCE: &[u8] = b"attest_key";
            let private_key = signing::derive_sr25519_key(NONCE);

            Self {
                owner: Self::env().caller(),
                attest_key: private_key[..32].try_into().expect("Invalid Key Length"),
                config: None,
                core_js: Default::default(),
            }
        }

        /// Gets the owner of the contract
        #[ink(message)]
        pub fn owner(&self) -> AccountId {
            self.owner
        }

        /// Gets the attestor address used by this rollup
        #[ink(message)]
        pub fn get_attest_address(&self) -> Vec<u8> {
            signing::get_public_key(&self.attest_key, signing::SigType::Sr25519)
        }

        /// Gets the ecdsa address used by this rollup in the meta transaction
        #[ink(message)]
        pub fn get_attest_ecdsa_address(&self) -> Vec<u8> {
            use ink::env::hash;
            let input = signing::get_public_key(&self.attest_key, signing::SigType::Ecdsa);
            let mut output = <hash::Blake2x256 as hash::HashOutput>::Type::default();
            ink::env::hash_bytes::<hash::Blake2x256>(&input, &mut output);
            output.to_vec()
        }

        /// Gets the sender address used by this rollup (in case of meta-transaction)
        #[ink(message)]
        pub fn get_sender_address(&self) -> Option<Vec<u8>> {
            if let Some(Some(sender_key)) = self.config.as_ref().map(|c| c.sender_key.as_ref()) {
                let sender_key = signing::get_public_key(sender_key, signing::SigType::Sr25519);
                Some(sender_key)
            } else {
                None
            }
        }

        /// Gets the config of the target consumer contract
        #[ink(message)]
        pub fn get_target_contract(&self) -> Option<(String, u8, u8, ContractId)> {
            self.config
                .as_ref()
                .map(|c| (c.rpc.clone(), c.pallet_id, c.call_id, c.contract_id))
        }

        /// Configures the target consumer contract (admin only)
        #[ink(message)]
        pub fn config_target_contract(
            &mut self,
            rpc: String,
            pallet_id: u8,
            call_id: u8,
            contract_id: Vec<u8>,
            sender_key: Option<Vec<u8>>,
        ) -> Result<()> {
            self.ensure_owner()?;
            self.config = Some(Config {
                rpc,
                pallet_id,
                call_id,
                contract_id: contract_id
                    .try_into()
                    .or(Err(ContractError::InvalidAddressLength))?,
                sender_key: match sender_key {
                    Some(key) => Some(key.try_into().or(Err(ContractError::InvalidKeyLength))?),
                    None => None,
                },
            });
            Ok(())
        }

        /// Get the core script
        #[ink(message)]
        pub fn get_core_js(&self) -> Option<CoreJs> {
            self.core_js.get()
        }

        /// Configures the core js (script + settings) (admin only)
        #[ink(message)]
        pub fn config_core_js(&mut self, script: String, settings: String) -> Result<()> {
            self.ensure_owner()?;
            self.config_core_js_inner(script, settings);
            Ok(())
        }

        /// Configures the core js (only script) (admin only)
        #[ink(message)]
        pub fn config_core_js_script(&mut self, script: String) -> Result<()> {
            self.ensure_owner()?;
            let Some(CoreJs { settings, .. }) = self.core_js.get() else {
                error!("CoreNotConfigured");
                return Err(ContractError::CoreNotConfigured);
            };
            self.config_core_js_inner(script, settings);
            Ok(())
        }

        /// Configures the core js (only script) (admin only)
        #[ink(message)]
        pub fn config_core_js_settings(&mut self, settings: String) -> Result<()> {
            self.ensure_owner()?;
            let Some(CoreJs { script, .. }) = self.core_js.get() else {
                error!("CoreNotConfigured");
                return Err(ContractError::CoreNotConfigured);
            };
            self.config_core_js_inner(script, settings);
            Ok(())
        }

        fn config_core_js_inner(&mut self, script: String, settings: String) {
            let code_hash = self
                .env()
                .hash_bytes::<ink::env::hash::Sha2x256>(script.as_bytes());
            let settings_hash = self
                .env()
                .hash_bytes::<ink::env::hash::Sha2x256>(settings.as_bytes());
            self.core_js.set(&CoreJs {
                script,
                settings,
                code_hash,
                settings_hash,
            });
        }

        /// Transfers the ownership of the contract (admin only)
        #[ink(message)]
        pub fn transfer_ownership(&mut self, new_owner: AccountId) -> Result<()> {
            self.ensure_owner()?;
            self.owner = new_owner;
            Ok(())
        }

        const NEXT_ERA: u32 = ink::selector_id!("NEXT_ERA");
        const NB_WINNERS: u32 = ink::selector_id!("NB_WINNERS");
        const LAST_WINNERS: u32 = ink::selector_id!("LAST_WINNER");

        /// Run the raffle
        #[ink(message)]
        pub fn run_raffle(&self) -> Result<Option<Vec<u8>>> {
            let config = self.ensure_client_configured()?;
            let mut client = connect(config)?;

            let era = client
                .get(&Self::NEXT_ERA)
                .log_err("run raffle: next era unknown")?
                .ok_or(ContractError::NextEraUnknown)?;

            let nb_winners = client
                .get(&Self::NB_WINNERS)
                .log_err("run raffle: nb winners not set")?
                .ok_or(ContractError::NbWinnersNotSet)?;

            let excluded: Vec<AccountId> = client
                .get(&Self::LAST_WINNERS)
                .log_err("run raffle: error when getting excluded addresses")?
                .unwrap_or_default();

            let request = RequestSc {
                era,
                nb_winners,
                excluded,
            };
            let response = self.handle_request(&request)?;
            // Attach an action to the tx by:
            client.action(Action::Reply(response.encode()));

            maybe_submit_tx(client, &self.attest_key, config.sender_key.as_ref())
        }

        /// Processes a request with the core js and returns the response.
        fn handle_request(&self, request_sc: &RequestSc) -> Result<ResponseMessage> {
            let Some(CoreJs {
                script,
                code_hash,
                settings,
                settings_hash,
            }) = self.core_js.get()
            else {
                error!("CoreNotConfigured");
                return Err(ContractError::CoreNotConfigured);
            };

            let request_js = convert_request(request_sc);
            let output_value_js = self.run_js_inner(&script, &request_js.encode(), settings)?;

            let input_hash = self
                .env()
                .hash_bytes::<ink::env::hash::Sha2x256>(&request_sc.encode());
            let response = ResponseMessage::JsResponse {
                js_script_hash: code_hash,
                input_hash,
                settings_hash,
                output_value: convert_output(output_value_js),
            };

            Ok(response)
        }

        /// Processes a request with the core js and returns the output.
        fn run_js_inner(&self, js_code: &str, request: &[u8], settings: String) -> Result<Vec<u8>> {
            let args = alloc::vec![alloc::format!("0x{}", hex_fmt::HexFmt(request)), settings];

            let output = phat_js::eval(js_code, &args)
                .log_err("Failed to eval the core js")
                .map_err(ContractError::JsError)?;

            let output_as_bytes = match output {
                phat_js::Output::String(s) => s.into_bytes(),
                phat_js::Output::Bytes(b) => b,
                phat_js::Output::Undefined => {
                    return Err(ContractError::JsError("Undefined output".to_string()))
                }
            };

            Ok(output_as_bytes)
        }
        /// Simulate the js
        ///
        /// For dev purpose. (admin only)
        #[ink(message)]
        pub fn dry_run_with_parameters(
            &self,
            era: u32,
            nb_winners: u16,
            excluded: Vec<AccountId>,
        ) -> Result<Vec<u8>> {
            self.ensure_owner()?;
            self.ensure_client_configured()?;
            let request = RequestSc {
                era,
                nb_winners,
                excluded,
            };
            let response = self.handle_request(&request)?;
            let encoded_response = response.encode();
            info!("encoded response : {:02x?}", encoded_response);
            Ok(encoded_response)
        }

        /// Simulate the js
        ///
        /// For dev purpose. (admin only)
        #[ink(message)]
        pub fn dry_run(&self) -> Result<Vec<u8>> {
            self.ensure_owner()?;

            let config = self.ensure_client_configured()?;
            let mut client = connect(config)?;

            let era = client
                .get(&Self::NEXT_ERA)
                .log_err("run raffle: next era unknown")?
                .ok_or(ContractError::NextEraUnknown)?;

            let nb_winners = client
                .get(&Self::NB_WINNERS)
                .log_err("run raffle: nb winners not set")?
                .ok_or(ContractError::NbWinnersNotSet)?;
            info!("nb_winners : {:?}", nb_winners);

            let excluded: Vec<AccountId> = client
                .get(&Self::LAST_WINNERS)
                .log_err("run raffle: error when getting excluded addresses")?
                .unwrap_or_default();
            info!("excluded : {:?}", excluded);

            self.dry_run_with_parameters(era, nb_winners, excluded)
        }

        /// Returns BadOrigin error if the caller is not the owner
        fn ensure_owner(&self) -> Result<()> {
            if self.env().caller() == self.owner {
                Ok(())
            } else {
                Err(ContractError::BadOrigin)
            }
        }

        /// Returns the config reference or raise the error `ClientNotConfigured`
        fn ensure_client_configured(&self) -> Result<&Config> {
            self.config
                .as_ref()
                .ok_or(ContractError::ClientNotConfigured)
        }
    }

    fn connect(config: &Config) -> Result<InkRollupClient> {
        let result = InkRollupClient::new(
            &config.rpc,
            config.pallet_id,
            config.call_id,
            &config.contract_id,
        )
        .log_err("failed to create rollup client");

        match result {
            Ok(client) => Ok(client),
            Err(e) => {
                error!("Error : {:?}", e);
                Err(ContractError::FailedToCreateClient)
            }
        }
    }

    fn maybe_submit_tx(
        client: InkRollupClient,
        attest_key: &[u8; 32],
        sender_key: Option<&[u8; 32]>,
    ) -> Result<Option<Vec<u8>>> {
        let maybe_submittable = client
            .commit()
            .log_err("failed to commit")
            .map_err(|_| ContractError::FailedToCommitTx)?;

        if let Some(submittable) = maybe_submittable {
            let tx_id = if let Some(sender_key) = sender_key {
                // Prefer to meta-tx
                submittable
                    .submit_meta_tx(attest_key, sender_key)
                    .log_err("failed to submit rollup meta-tx")?
            } else {
                // Fallback to account-based authentication
                submittable
                    .submit(attest_key)
                    .log_err("failed to submit rollup tx")?
            };
            return Ok(Some(tx_id));
        }
        Ok(None)
    }

    #[derive(Encode, Decode)]
    pub struct RequestSc {
        era: u32,
        nb_winners: u16,
        excluded: Vec<AccountId>,
    }

    #[derive(Encode, Decode)]
    pub struct RequestJs {
        era: u32,
        nb_winners: u16,
        excluded: Vec<String>,
    }

    fn convert_address_input(address: &AccountId) -> String {
        let address_hex: [u8; 32] = scale::Encode::encode(&address)
            .try_into()
            .expect("incorrect length");
        AccountId32::from(address_hex)
            .to_ss58check_with_version(Ss58AddressFormatRegistry::AstarAccount.into())
    }

    fn convert_request(request_sc: &RequestSc) -> RequestJs {
        let era = request_sc.era;
        let nb_winners = request_sc.nb_winners;
        let excluded = request_sc
            .excluded
            .iter()
            .map(convert_address_input)
            .collect();
        RequestJs {
            era,
            nb_winners,
            excluded,
        }
    }

    #[derive(scale::Encode, scale::Decode)]
    pub struct ResponseJs {
        pub era: u32,
        pub skipped: bool,
        pub rewards: Balance,
        pub winners: Vec<String>,
    }

    #[derive(scale::Encode, scale::Decode)]
    pub struct ResponseSc {
        pub era: u32,
        pub skipped: bool,
        pub rewards: Balance,
        pub winners: Vec<AccountId>,
    }

    fn convert_address_output(address: &str) -> AccountId {
        let account_id = AccountId32::from_ss58check(address).expect("incorrect address");
        let address_hex: [u8; 32] = scale::Encode::encode(&account_id)
            .try_into()
            .expect("incorrect length");
        AccountId::from(address_hex)
    }

    fn convert_output(output: Vec<u8>) -> Vec<u8> {
        let output_js =
            ResponseJs::decode(&mut output.as_slice()).expect("failed to convert js output");
        let era = output_js.era;
        let skipped = output_js.skipped;
        let rewards = output_js.rewards;
        let winners = output_js
            .winners
            .iter()
            .map(|s| convert_address_output(s.as_str()))
            .collect();
        let output_sc = ResponseSc {
            era,
            skipped,
            rewards,
            winners,
        };

        output_sc.encode()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        struct EnvVars {
            /// The RPC endpoint of the target blockchain
            rpc: String,
            pallet_id: u8,
            call_id: u8,
            /// The rollup anchor address on the target blockchain
            contract_id: ContractId,
            /// When we want to manually set the attestor key for signing the message (only dev purpose)
            attest_key: Vec<u8>,
            /// When we want to use meta tx
            sender_key: Option<Vec<u8>>,
        }

        fn get_env(key: &str) -> String {
            std::env::var(key).expect("env not found")
        }

        fn config() -> EnvVars {
            dotenvy::dotenv().ok();
            let rpc = get_env("RPC");
            let pallet_id: u8 = get_env("PALLET_ID").parse().expect("u8 expected");
            let call_id: u8 = get_env("CALL_ID").parse().expect("u8 expected");
            let contract_id: ContractId = hex::decode(get_env("CONTRACT_ID"))
                .expect("hex decode failed")
                .try_into()
                .expect("incorrect length");
            let attest_key = hex::decode(get_env("ATTEST_KEY")).expect("hex decode failed");
            let sender_key = std::env::var("SENDER_KEY")
                .map(|s| hex::decode(s).expect("hex decode failed"))
                .ok();

            EnvVars {
                rpc: rpc.to_string(),
                pallet_id,
                call_id,
                contract_id: contract_id.into(),
                attest_key,
                sender_key,
            }
        }

        fn init_contract() -> JsOffchainRollup {
            let EnvVars {
                rpc,
                pallet_id,
                call_id,
                contract_id,
                attest_key,
                sender_key,
            } = config();

            let mut oracle = JsOffchainRollup::default();
            oracle
                .config_target_contract(rpc, pallet_id, call_id, contract_id.into(), sender_key)
                .unwrap();
            //oracle.set_attest_key(Some(attest_key)).unwrap();

            oracle
        }

        #[ink::test]
        #[ignore = "The JS Contract is not accessible inner the test"]
        fn run_raffle() {
            let _ = env_logger::try_init();
            pink_extension_runtime::mock_ext::mock_all_ext();

            let oracle = init_contract();

            let r = oracle.run_raffle().expect("failed to run raffle");
            ink::env::debug_println!("answer request: {r:?}");
        }

        #[ink::test]
        fn test_convert_address() {
            let _ = env_logger::try_init();
            pink_extension_runtime::mock_ext::mock_all_ext();

            let address_hex: [u8; 32] =
                hex::decode("bc5a6b58324a633175374b57464a42357476554b3364774e4673454132436e66")
                    .expect("hex decode failed")
                    .try_into()
                    .expect("incorrect length");
            let address = AccountId::from(address_hex);

            let astar_address_str = convert_address_input(&address);
            assert_eq!(
                astar_address_str,
                "aCG9z4XcZrSUfrzuaUYWwxKruA6rnA8z9wMcZtDQEfPRQLH"
            );

            assert_eq!(address, convert_address_output(&astar_address_str));
        }

        #[ink::test]
        fn test_encode_input() {
            let _ = env_logger::try_init();
            pink_extension_runtime::mock_ext::mock_all_ext();

            let era = 4517;
            let nb_winners = 2;
            let mut excluded = Vec::new();
            let address1_hex: [u8; 32] =
                hex::decode("bc5a6b58324a633175374b57464a42357476554b3364774e4673454132436e66")
                    .expect("hex decode failed")
                    .try_into()
                    .expect("incorrect length");
            let address1 = AccountId::from(address1_hex);
            excluded.push(address1);

            let address2_hex: [u8; 32] =
                hex::decode("bf80905d9c52857f94b92b8771568687c251c6e9784ec0aff0d3e2ce0374b948")
                    .expect("hex decode failed")
                    .try_into()
                    .expect("incorrect length");
            let address2 = AccountId::from(address2_hex);
            excluded.push(address2);

            let request_sc = RequestSc {
                era,
                nb_winners,
                excluded,
            };
            let request_js = convert_request(&request_sc);
            let encoded_request = scale::Encode::encode(&request_js);
            ink::env::debug_println!("encoded request: {encoded_request:02x?}");
        }
    }

    #[ink::test]
    fn test_encode_output() {
        let _ = env_logger::try_init();
        pink_extension_runtime::mock_ext::mock_all_ext();

        let address_string = "VzfTbmS78JcknxhvdgrxGRHeTqkGdh8o35PfiLSM7cZbMAC".to_string();
        let response_sc = ResponseJs {
            era: 5015,
            skipped: false,
            rewards: 163483092786717962675,
            winners: vec![address_string],
        };

        let response = convert_output(response_sc.encode());
        ink::env::debug_println!("output: {response:02x?}");
    }

    #[ink::test]
    fn test_encode_reply() {
        let _ = env_logger::try_init();
        pink_extension_runtime::mock_ext::mock_all_ext();

        let era = 5016;
        let nb_winners = 3;
        let mut excluded = Vec::new();
        // Z5VAv3wwBuH15d1gAgjztz8CQPG6rwekFYxavgjhggJicsk
        excluded.push(convert_address(
            "8af348b187a2e94f7dfcacc1de5c71b55f6ab8a50e75f0ac1a15baeebfd92e03",
        ));
        // ZjyoCabAjvNo9Evx1jc4Kysi3mG5ynchMcn6JPkR5c4SiYh
        excluded.push(convert_address(
            "a84ef8c0efdd519e001cb3f5b6351725178283edf1b122a464f0a0f425699759",
        ));
        // beVJDH4QHwagLCv9LjHAQA2ZPUesQXF4BQvoGQ68Yt16sso
        excluded.push(convert_address(
            "fc9745d14123e9ad945375d5681ebc3266e45a7ba5924adf9a061b4c8951c210",
        ));
        // ZSL4XKCjjobePpeQZRLVkEboWT3HFezuKEZabK9viSsJsGc
        excluded.push(convert_address(
            "9ad8c50ef2cf1ef56b9129e1d471d897018d1314c3618fa9c0a75f875bd16c68",
        ));
        // XwPQ6Zj4Y3UM1vFEyMwPiNCpmbtYHLe8TXHdNVp7fU6qYmj
        excluded.push(convert_address(
            "5889a78dc053a819141e39ab80638a30ea71edcadff45abc663973817b648c33",
        ));
        // X2WUWWpxJP4aaQnkdftwgziX29U6YmR6EAV4Z4ck2SiBf7v
        excluded.push(convert_address(
            "303577e2947dbcc5a1189e4bf527c4f8ec54aa0fcfb4e1a07bfdb9928aa70f64",
        ));
        // YSS3vdcp8EoThvdDb32CmZYMaGaQUfTCvpH3paWJ1Ddc7Ym
        excluded.push(convert_address(
            "6eb0b30beb0726ae75d11f781f0b8a7d56636f6bd36c45d8892248ea2d800a66",
        ));
        // VzfTbmS78JcknxhvdgrxGRHeTqkGdh8o35PfiLSM7cZbMAC
        excluded.push(convert_address(
            "0290fceaac42bbfcd509a936c6e7e91f1cb92b9f9b12c5ed83b2da58dfcc7056",
        ));

        let request_sc = RequestSc {
            era,
            nb_winners,
            excluded,
        };
        let request_js = convert_request(&request_sc);
        let encoded_request = scale::Encode::encode(&request_js);
        ink::env::debug_println!("encoded request: {encoded_request:02x?}");
        //"0x97130000010020bc6177344a4733446f58364b5241424a4866696b59687938696e576176706d7931465a4a6955596f31537a3778456437bc5a35564176337777427548313564316741676a7a747a3843515047367277656b4659786176676a6867674a6963736bbc5a6a796f436162416a764e6f39457678316a63344b797369336d4735796e63684d636e364a506b5235633453695968bc6265564a44483451487761674c4376394c6a48415141325a50556573515846344251766f475136385974313673736fbc5a534c34584b436a6a6f6265507065515a524c566b45626f5754334846657a754b455a61624b39766953734a734763bc58775051365a6a345933554d31764645794d7750694e43706d627459484c6538545848644e56703766553671596d6abc58325755575770784a50346161516e6b64667477677a695832395536596d5236454156345a34636b32536942663776bc595353337664637038456f5468766444623332436d5a594d6147615155665443767048337061574a3144646337596d"

        let response_sc = ResponseJs {
            era,
            skipped: false,
            rewards: 163483092786717962675,
            winners: vec![
                "WAsEdTzXTfCmweU62WGyDvcjcV9u4qd6FrHEfwoEj6f4z4f".to_string(),
                "ajYMsCKsEAhEvHpeA4XqsfiA9v1CdzZPrCfS6pEfeGHW9j8".to_string(),
                "ZAP5o2BjWAo5uoKDE6b6Xkk4Ju7k6bDu24LNjgZbfM3iyiR".to_string(),
            ],
        };

        let encoded_response = convert_output(response_sc.encode());
        ink::env::debug_println!("encoded response js: {encoded_response:02x?}");

        let mut input_hash =
            <ink::env::hash::Sha2x256 as ink::env::hash::HashOutput>::Type::default();
        ink::env::hash_bytes::<ink::env::hash::Sha2x256>(&request_sc.encode(), &mut input_hash);

        let js_script_hash: [u8; 32] =
            hex::decode("6e0fa1cd2780cb1d2c6af3f8fb56dffc37198923009f7756796edf6c9c6ab464")
                .expect("hex decode failed")
                .try_into()
                .expect("incorrect length");

        let settings_hash: [u8; 32] =
            hex::decode("0c7bf54e0c2cc651d2527d412c18baa72feea59260fb9d2efbbf2cab05d6ec1a")
                .expect("hex decode failed")
                .try_into()
                .expect("incorrect length");

        let response_message = ResponseMessage::JsResponse {
            js_script_hash,
            input_hash,
            settings_hash,
            output_value: encoded_response,
        };

        let encoded_response_message = response_message.encode();
        ink::env::debug_println!("encoded response message: {encoded_response_message:02x?}");
    }

    //fn convert_address(public_key: String) -> AccountId {
    fn convert_address<T: AsRef<[u8]>>(public_key: T) -> AccountId {
        let address_hex: [u8; 32] = hex::decode(public_key)
            .expect("hex decode failed")
            .try_into()
            .expect("incorrect length");
        AccountId::from(address_hex)
    }
}
