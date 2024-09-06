use actix_web::web::Data;
use actix_web::{
    body::MessageBody,
    dev::{ServiceFactory, ServiceRequest, ServiceResponse},
    App, Error,
};
use anyhow::Result;
use ethers::prelude::*;
use ethers::types::{Address, H160};
use ethers::utils::{keccak256, public_key_to_address};
use k256::ecdsa::SigningKey;
use rand::rngs::OsRng;
use serde_json::json;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::Mutex;

use crate::api_impl::{
    export_signed_registration_message, get_gateway_details, index, inject_immutable_config,
    inject_mutable_config,
};
use crate::chain_util::HttpProviderLogs;
use crate::constant::{
    COMMON_CHAIN_JOB_RELAYED_EVENT, REQUEST_CHAIN_JOB_SUBSCRIPTION_STARTED_EVENT,
};
use crate::error::ServerlessError;
use crate::model::{AppState, ContractsClient, Job};

// Testnet or Local blockchain (Hardhat) configurations
#[cfg(test)]
pub const CHAIN_ID: u64 = 421614;
#[cfg(test)]
const HTTP_RPC_URL: &str = "https://sepolia-rollup.arbitrum.io/rpc";
#[cfg(test)]
const WS_URL: &str = "wss://arbitrum-sepolia.infura.io/ws/v3/cd72f20b9fd544f8a5b8da706441e01c";
#[cfg(test)]
const GATEWAY_CONTRACT_ADDR: &str = "0x9a79Bb5676c19A01ad27D88ca6A0131d51022AC4";
#[cfg(test)]
pub const GATEWAY_JOBS_CONTRACT_ADDR: &str = "0x124371e1E13f2917A73E8eca9F361e6aA21eA06a";
#[cfg(test)]
pub const RELAY_CONTRACT_ADDR: &str = "0x1Af94DA972cC2B12dbfcb2871d62e531e4d4f1F0";
#[cfg(test)]
const SUBSCRIPTION_RELAY_CONTRACT_ADDR: &str = "0xA37F74824dA3DDaF241461c11f069Ebd2cc44b1a";
#[cfg(test)]
pub const OWNER_ADDRESS: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
#[cfg(test)]
pub const GAS_WALLET_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
#[cfg(test)]
pub const GAS_WALLET_PUBLIC_ADDRESS: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
#[cfg(test)]
const EPOCH: u64 = 1713433800;
#[cfg(test)]
const TIME_INTERVAL: u64 = 300;
#[cfg(test)]
const OFFSET_FOR_EPCOH: u64 = 20;

#[cfg(test)]
pub fn new_app(
    app_state: Data<AppState>,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Response = ServiceResponse<impl MessageBody + std::fmt::Debug>,
        Config = (),
        InitError = (),
        Error = Error,
    >,
> {
    App::new()
        .app_data(app_state)
        .service(index)
        .service(inject_immutable_config)
        .service(inject_mutable_config)
        .service(export_signed_registration_message)
        .service(get_gateway_details)
}

#[cfg(test)]
pub async fn generate_app_state() -> Data<AppState> {
    // Initialize random 'secp256k1' signing key for the enclave
    let signer_key = SigningKey::random(&mut OsRng);

    Data::new(AppState {
        enclave_signer_key: signer_key.clone(),
        enclave_address: public_key_to_address(&signer_key.verifying_key()),
        wallet: None.into(),
        common_chain_id: CHAIN_ID,
        common_chain_http_url: HTTP_RPC_URL.to_owned(),
        common_chain_ws_url: WS_URL.to_owned(),
        gateways_contract_addr: GATEWAY_CONTRACT_ADDR.parse::<Address>().unwrap(),
        gateway_jobs_contract_addr: GATEWAY_JOBS_CONTRACT_ADDR.parse::<Address>().unwrap(),
        request_chain_ids: HashSet::new().into(),
        registered: Arc::new(AtomicBool::new(false)),
        registration_events_listener_active: false.into(),
        epoch: EPOCH,
        time_interval: TIME_INTERVAL,
        offset_for_epoch: OFFSET_FOR_EPCOH,
        enclave_owner: H160::zero().into(),
        immutable_params_injected: Mutex::new(false),
        mutable_params_injected: Arc::new(AtomicBool::new(false)),
        contracts_client: Mutex::new(None),
    })
}

#[cfg(test)]
pub async fn generate_contracts_client() -> Arc<ContractsClient> {
    let app_state = generate_app_state().await;
    let app = actix_web::test::init_service(new_app(app_state.clone())).await;

    // add immutable config
    let req = actix_web::test::TestRequest::post()
        .uri("/immutable-config")
        .set_json(&json!({
            "owner_address_hex": OWNER_ADDRESS
        }))
        .to_request();
    actix_web::test::call_service(&app, req).await;

    // add mutable config
    let req = actix_web::test::TestRequest::post()
        .uri("/mutable-config")
        .set_json(&json!({
            "gas_key_hex": GAS_WALLET_KEY
        }))
        .to_request();
    actix_web::test::call_service(&app, req).await;

    // Get signature with valid data points
    let req = actix_web::test::TestRequest::get()
        .uri("/signed-registration-message")
        .set_json(&json!({
            "chain_ids": [CHAIN_ID]
        }))
        .to_request();

    actix_web::test::call_service(&app, req).await;

    let contracts_client = app_state.contracts_client.lock().unwrap().clone().unwrap();

    contracts_client
}

#[cfg(test)]
pub struct MockHttpProvider {
    pub job: Job,
}

#[cfg(test)]
impl MockHttpProvider {
    pub fn new(job: Job) -> Self {
        Self { job }
    }
}

#[cfg(test)]
impl HttpProviderLogs for MockHttpProvider {
    async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>, ServerlessError> {
        use ethers::abi::{encode, Token};
        use ethers::prelude::*;
        use serde_json::json;
        use std::time::{SystemTime, UNIX_EPOCH};

        if let Some(topic0) = filter.topics().next() {
            let topic0 = match topic0 {
                ValueOrArray::Value(s) => *s,
                ValueOrArray::Array(s) => s[0],
            };
            if topic0.is_none() {
                return Err(ServerlessError::EmptyTopic0);
            }

            let topic0 = topic0.unwrap();

            // Mock logs for gateways_job_relayed_logs
            if topic0.eq(&H256::from_slice(&keccak256(
                COMMON_CHAIN_JOB_RELAYED_EVENT,
            ))) {
                if self.job.job_id == U256::from(1) {
                    Ok(vec![Log {
                        address: H160::from_str(GATEWAY_JOBS_CONTRACT_ADDR).unwrap().into(),
                        topics: vec![
                            keccak256(COMMON_CHAIN_JOB_RELAYED_EVENT).into(),
                            H256::from_uint(&self.job.job_id),
                        ],
                        data: encode(&[
                            Token::Uint(U256::from(100)),
                            Token::Address(self.job.job_owner),
                            Token::Address(self.job.gateway_address.unwrap()),
                        ])
                        .into(),
                        ..Default::default()
                    }])
                } else {
                    Ok(vec![Log {
                        address: Address::default(),
                        topics: vec![H256::default(), H256::default(), H256::default()],
                        data: Bytes::default(),
                        ..Default::default()
                    }])
                }
            }
            // Mock logs for request_chain_job_subscription_started_event
            else if topic0.eq(&H256::from_slice(&keccak256(
                REQUEST_CHAIN_JOB_SUBSCRIPTION_STARTED_EVENT,
            ))) {
                let job_subscriber = Address::random();
                let system_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                let subscription_started_still_active_event = Log {
                    address: Address::default(),
                    topics: vec![
                        keccak256(REQUEST_CHAIN_JOB_SUBSCRIPTION_STARTED_EVENT).into(),
                        H256::from_uint(&U256::from(1)),
                        H256::from(job_subscriber),
                    ],
                    data: encode(&[
                        Token::Uint(U256::from(10)),
                        Token::Uint(U256::from(1000)),
                        Token::Uint(U256::from(system_time + 1000)),
                        Token::Uint(U256::from(100)),
                        Token::Address(Address::random()),
                        Token::FixedBytes(
                            hex::decode(
                                "9468bb6a8e85ed11e292c8cac0c1539df691c8d8ec62e7dbfa9f1bd7f504e46e"
                                    .to_owned(),
                            )
                            .unwrap(),
                        ),
                        Token::Bytes(
                            serde_json::to_vec(&json!({
                                "num": 10
                            }))
                            .unwrap(),
                        ),
                        Token::Uint(U256::from(system_time)),
                    ])
                    .into(),
                    ..Default::default()
                };

                Ok(vec![subscription_started_still_active_event])
            } else {
                return Err(ServerlessError::InvalidTopic);
            }
        } else {
            return Err(ServerlessError::EmptyTopics);
        }
    }
}
