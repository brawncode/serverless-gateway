mod api_impl;
mod chain_util;
mod common_chain_gateway_state_service;
mod common_chain_interaction;
mod config;
mod constant;
mod contract_abi;
mod model;

use actix_web::web::Data;
use actix_web::{App, HttpServer};
use anyhow::Context;
use clap::Parser;
use env_logger::Env;
use ethers::prelude::*;
use ethers::providers::Provider;
use k256::ecdsa::SigningKey;
use std::collections::BTreeMap;
use std::error::Error;
use std::sync::Arc;
use std::sync::RwLock;
use tokio::fs;

use crate::api_impl::{deregister_enclave, index, inject_key, register_enclave};
use crate::model::{AppState, ConfigManager, GatewayData};

type HttpProvider = NonceManagerMiddleware<SignerMiddleware<Provider<Http>, LocalWallet>>;

#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    #[clap(
        long,
        value_parser,
        default_value = "./oyster_serverless_gateway_config.json"
    )]
    config_file: String,
    #[clap(long, value_parser, default_value = "6001")]
    port: u16,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    // Load the configuration file
    let args = Cli::parse();
    let config_manager = ConfigManager::new(&args.config_file);
    let config = config_manager.load_config().unwrap();

    let enclave_pub_key = fs::read(config.enclave_public_key)
        .await
        .context("Failed to read the enclave signer key")?;

    let enclave_signer_key = SigningKey::from_slice(
        fs::read(config.enclave_secret_key)
            .await
            .context("Failed to read the enclave signer key")?
            .as_slice(),
    )
    .context("Invalid enclave signer key")?;

    let gateway_epoch_state: Arc<RwLock<BTreeMap<u64, BTreeMap<Address, GatewayData>>>> =
        Arc::new(RwLock::new(BTreeMap::new()));

    // Create a Appstate
    let app_data = Data::new(AppState {
        enclave_signer_key,
        wallet: None.into(),
        common_chain_id: config.com_chain_id,
        common_chain_http_url: config.com_chain_http_url,
        common_chain_ws_url: config.com_chain_ws_url,
        gateway_contract_addr: config.gateway_contract_addr,
        job_contract_addr: config.job_contract_addr,
        chain_list: vec![].into(),
        registered: false.into(),
        enclave_pub_key: enclave_pub_key.into(),
        gateway_epoch_state,
        epoch: config.epoch,
        time_interval: config.time_interval,
    });
    // Start a http server
    let server = HttpServer::new(move || {
        App::new()
            .app_data(app_data.clone())
            .service(index)
            .service(inject_key)
            .service(register_enclave)
            .service(deregister_enclave)
    })
    .bind(("0.0.0.0", args.port))
    .context(format!("could not bind to port {}", args.port))?
    .run();

    println!("Node server started on port {}", args.port);

    server.await?;

    Ok(())
}
