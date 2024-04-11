use anyhow::{anyhow, Context, Result};
use ethers::abi::{decode, ParamType, Token};
use ethers::prelude::*;
use ethers::utils::keccak256;
use log::error;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time;

use crate::common_chain_util::get_block_number_by_timestamp;
use crate::constant::GATEWAY_BLOCK_STATES_TO_MAINTAIN;

#[derive(Debug, Clone)]
pub struct GatewayData {
    pub last_block_number: u64,
    pub enclave_pub_key: Bytes,
    pub address: Address,
    pub stake_amount: U256,
    pub status: bool,
    pub req_chain_ids: BTreeSet<U256>,
}

// Initialize the gateway epoch state
pub async fn gateway_epoch_state_service(
    contract_address: Address,
    provider: &Provider<Http>,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
    epoch: u64,
    time_interval: u64,
) {
    let current_cycle = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - epoch)
        / time_interval;

    let initial_epoch_cycle: u64;
    if current_cycle >= GATEWAY_BLOCK_STATES_TO_MAINTAIN {
        initial_epoch_cycle = current_cycle - GATEWAY_BLOCK_STATES_TO_MAINTAIN + 1;
    } else {
        initial_epoch_cycle = 1;
    };
    {
        let contract_address_clone = contract_address.clone();
        let provider_clone = provider.clone();
        let gateway_epoch_state_clone = Arc::clone(gateway_epoch_state);
        tokio::spawn(async move {
            for cycle_number in initial_epoch_cycle..=current_cycle {
                generate_gateway_epoch_state_for_cycle(
                    contract_address_clone,
                    &provider_clone,
                    &gateway_epoch_state_clone,
                    cycle_number,
                    epoch,
                    time_interval,
                )
                .await
                .unwrap();
            }
        });
    }

    let mut cycle_number = current_cycle + 1;
    let last_cycle_timestamp = epoch + (current_cycle * time_interval);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    let until_epoch = Duration::from_secs(last_cycle_timestamp + time_interval);

    if until_epoch > now {
        let sleep_duration = until_epoch - now;
        tokio::time::sleep(sleep_duration).await;
    }

    let mut interval = time::interval(Duration::from_secs(time_interval));

    loop {
        interval.tick().await;

        generate_gateway_epoch_state_for_cycle(
            contract_address,
            provider,
            gateway_epoch_state,
            cycle_number,
            epoch,
            time_interval,
        )
        .await
        .unwrap();

        prune_old_cycle_states(gateway_epoch_state, cycle_number).await;

        cycle_number += 1;
    }
}

// TODO: if fails, add a retry mechanism
pub async fn generate_gateway_epoch_state_for_cycle(
    contract_address: Address,
    provider: &Provider<Http>,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
    cycle_number: u64,
    epoch: u64,
    time_interval: u64,
) -> Result<()> {
    let mut last_added_cycle: Option<u64> = None;
    let added_cycles: Vec<u64>;
    // scope for the read lock
    {
        let gateway_epoch_state_guard = gateway_epoch_state.read().await;
        added_cycles = gateway_epoch_state_guard.keys().cloned().collect();
    }
    for cycle in added_cycles.iter().rev() {
        if *cycle == cycle_number {
            return Ok(());
        } else if *cycle < cycle_number {
            last_added_cycle = Some(cycle.clone());
        }
    }
    drop(added_cycles);

    let from_block_number: u64;

    if last_added_cycle.is_none() {
        from_block_number = 0;
    } else {
        // scope for the read lock
        {
            // get last added cycle's block number
            from_block_number = gateway_epoch_state
                .read()
                .await
                .get(&last_added_cycle.unwrap())
                .unwrap()
                .values()
                .next()
                .unwrap()
                .last_block_number
                + 1;
        }
    }

    let timestamp_to_fetch = epoch + (cycle_number * time_interval);

    // TODO: handle the case when to_block_number is less than from_block_number
    let to_block_number =
        get_block_number_by_timestamp(&provider, timestamp_to_fetch, from_block_number).await;

    if to_block_number.is_none() {
        error!(
            "Failed to get block number for timestamp {}",
            timestamp_to_fetch
        );
        return Err(anyhow!(
            "Failed to get block number for timestamp {}",
            timestamp_to_fetch
        ));
    }

    let to_block_number = to_block_number.unwrap();

    if last_added_cycle.is_none() {
        // initialize the gateway epoch state[current_cycle] with empty map
        // scope for the write lock
        {
            let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
            gateway_epoch_state_guard.insert(cycle_number, BTreeMap::new());
        }
    } else {
        // initialize the gateway epoch state[current_cycle] with the previous cycle state
        let last_cycle_state_map: BTreeMap<Bytes, GatewayData>;
        // scope for the read lock
        {
            last_cycle_state_map = gateway_epoch_state
                .read()
                .await
                .get(&last_added_cycle.unwrap())
                .unwrap()
                .clone();
        }
        // update the last block number of the gateway data
        for gateway_data in last_cycle_state_map.values() {
            // scope for the write lock
            {
                let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
                gateway_epoch_state_guard
                    .get_mut(&cycle_number)
                    .unwrap()
                    .insert(
                        gateway_data.enclave_pub_key.clone(),
                        GatewayData {
                            last_block_number: to_block_number.clone(),
                            enclave_pub_key: gateway_data.enclave_pub_key.clone(),
                            address: gateway_data.address.clone(),
                            stake_amount: gateway_data.stake_amount.clone(),
                            status: gateway_data.status.clone(),
                            req_chain_ids: gateway_data.req_chain_ids.clone(),
                        },
                    );
            }
        }
        // scope for the write lock
        {
            let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
            gateway_epoch_state_guard.insert(cycle_number, last_cycle_state_map);
        }
    }

    let event_filter = Filter::new()
        .address(contract_address)
        .from_block(from_block_number)
        .to_block(to_block_number)
        .topic0(vec![
            keccak256("GatewayRegistered(bytes,address,address,uint256,uint256[])"),
            keccak256("GatewayDeregistered(bytes)"),
            keccak256("GatewayStakeAdded(bytes,uint256,uint256)"),
            keccak256("GatewayStakeRemoved(bytes,uint256,uint256)"),
            keccak256("ChainAdded(bytes,uint256)"),
            keccak256("ChainRemoved(bytes,uint256)"),
        ]);

    let logs = provider
        .get_logs(&event_filter)
        .await
        .context("Failed to get logs for the gateway contract")
        .unwrap();

    for log in logs {
        let topics = log.topics.clone();

        if topics[0]
            == keccak256("GatewayRegistered(bytes,address,address,uint256,uint256[])").into()
        {
            process_gateway_registered_event(
                log,
                cycle_number,
                to_block_number,
                &gateway_epoch_state,
            )
            .await;
        } else if topics[0] == keccak256("GatewayDeregistered(bytes)").into() {
            process_gateway_deregistered_event(log, to_block_number, &gateway_epoch_state).await;
        } else if topics[0] == keccak256("GatewayStakeAdded(bytes,uint256,uint256)").into() {
            process_gateway_stake_added_event(log, cycle_number, &gateway_epoch_state).await;
        } else if topics[0] == keccak256("GatewayStakeRemoved(bytes,uint256,uint256)").into() {
            process_gateway_stake_removed_event(log, cycle_number, &gateway_epoch_state).await;
        } else if topics[0] == keccak256("ChainAdded(bytes,uint256)").into() {
            process_chain_added_event(log, cycle_number, &gateway_epoch_state).await;
        } else if topics[0] == keccak256("ChainRemoved(bytes,uint256)").into() {
            process_chain_removed_event(log, cycle_number, &gateway_epoch_state).await;
        }
    }

    // TODO: fetch the gateways mapping for the updated stakes.

    Ok(())
}

async fn prune_old_cycle_states(
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
    current_cycle: u64,
) {
    let mut cycles_to_remove = vec![];

    // scope for the read lock
    {
        let gateway_epoch_state_guard = gateway_epoch_state.read().await;
        for cycle in gateway_epoch_state_guard.keys() {
            // if a state is older than 1.5 times the number of states to maintain, remove it
            // chosen a number larger than the number to maintain because in some cases, of delay,
            // an older state might be used to read and initialize the current state
            if current_cycle - cycle >= (GATEWAY_BLOCK_STATES_TO_MAINTAIN * 3 / 2) {
                cycles_to_remove.push(cycle.clone());
            } else {
                break;
            }
        }
    }
    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        for cycle in cycles_to_remove {
            gateway_epoch_state_guard.remove(&cycle);
        }
    }
}

async fn process_gateway_registered_event(
    log: Log,
    cycle: u64,
    to_block_number: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(
        &vec![
            ParamType::Bytes,
            ParamType::Address,
            ParamType::Address,
            ParamType::Uint(256),
            ParamType::Array(Box::new(ParamType::Uint(256))),
        ],
        &log.data.0,
    )
    .unwrap();

    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();
    let address = decoded[1].clone().into_address().unwrap();
    let stake_amount = decoded[3].clone().into_uint().unwrap();
    let mut req_chain_ids = BTreeSet::new();

    if let Token::Array(array_tokens) = decoded[4].clone() {
        for token in array_tokens {
            if let Token::Uint(req_chain_id) = token {
                req_chain_ids.insert(req_chain_id.clone());
            }
        }
    }

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            cycle_gateway_state.insert(
                enclave_pub_key.clone(),
                GatewayData {
                    last_block_number: to_block_number,
                    enclave_pub_key,
                    address,
                    stake_amount,
                    status: true,
                    req_chain_ids,
                },
            );
        }
    }
}

async fn process_gateway_deregistered_event(
    log: Log,
    cycle: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(&vec![ParamType::Bytes], &log.data.0).unwrap();
    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            cycle_gateway_state.remove(&enclave_pub_key);
        }
    }
}

async fn process_gateway_stake_added_event(
    log: Log,
    cycle: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(
        &vec![ParamType::Bytes, ParamType::Uint(256), ParamType::Uint(256)],
        &log.data.0,
    )
    .unwrap();
    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();
    let total_stake_amount = decoded[2].clone().into_uint().unwrap();

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            if let Some(gateway_data) = cycle_gateway_state.get_mut(&enclave_pub_key) {
                gateway_data.stake_amount = total_stake_amount;
            }
        }
    }
}

async fn process_gateway_stake_removed_event(
    log: Log,
    cycle: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(
        &vec![ParamType::Bytes, ParamType::Uint(256), ParamType::Uint(256)],
        &log.data.0,
    )
    .unwrap();
    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();
    let total_stake_amount = decoded[2].clone().into_uint().unwrap();

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            if let Some(gateway_data) = cycle_gateway_state.get_mut(&enclave_pub_key) {
                gateway_data.stake_amount = total_stake_amount;
            }
        }
    }
}

async fn process_chain_added_event(
    log: Log,
    cycle: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(&vec![ParamType::Bytes, ParamType::Uint(256)], &log.data.0).unwrap();
    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();
    let chain_id = decoded[1].clone().into_uint().unwrap();

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            if let Some(gateway_data) = cycle_gateway_state.get_mut(&enclave_pub_key) {
                gateway_data.req_chain_ids.insert(chain_id);
            }
        }
    }
}

async fn process_chain_removed_event(
    log: Log,
    cycle: u64,
    gateway_epoch_state: &Arc<RwLock<BTreeMap<u64, BTreeMap<Bytes, GatewayData>>>>,
) {
    let decoded = decode(&vec![ParamType::Bytes, ParamType::Uint(256)], &log.data.0).unwrap();
    let enclave_pub_key: Bytes = decoded[0].clone().into_bytes().unwrap().into();
    let chain_id = decoded[1].clone().into_uint().unwrap();

    // scope for the write lock
    {
        let mut gateway_epoch_state_guard = gateway_epoch_state.write().await;
        if let Some(cycle_gateway_state) = gateway_epoch_state_guard.get_mut(&cycle) {
            if let Some(gateway_data) = cycle_gateway_state.get_mut(&enclave_pub_key) {
                gateway_data.req_chain_ids.remove(&chain_id);
            }
        }
    }
}
