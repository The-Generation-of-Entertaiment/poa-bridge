use std::sync::{Arc, RwLock};
use futures::{self, Future, Stream, Poll};
use futures::future::{JoinAll, join_all};
use tokio_timer::Timeout;
use web3::Transport;
use web3::types::{H256, U256, Address, Bytes, Log, FilterBuilder};
use ethabi::RawLog;
use api::{LogStream, self, ApiCall};
use error::{Error, ErrorKind, Result};
use database::Database;
use contracts::{home, foreign};
use util::web3_filter;
use app::App;
use transaction::prepare_raw_transaction;
use ethcore_transaction::{Transaction, Action};
use itertools::Itertools;

fn deposits_filter(home: &home::HomeBridge, address: Address) -> FilterBuilder {
	let filter = home.events().deposit().create_filter();
	web3_filter(filter, address)
}

fn deposit_relay_payload(home: &home::HomeBridge, foreign: &foreign::ForeignBridge, log: Log) -> Result<Bytes> {
	let raw_log = RawLog {
		topics: log.topics,
		data: log.data.0,
	};
	let deposit_log = home.events().deposit().parse_log(raw_log)?;
	let hash = log.transaction_hash.expect("log to be mined and contain `transaction_hash`");
	let payload = foreign.functions().deposit().input(deposit_log.recipient, deposit_log.value, hash.0);
	Ok(payload.into())
}

/// State of deposits relay.
enum DepositRelayState<T: Transport> {
	/// Deposit relay is waiting for logs.
	Wait,
	/// Relaying deposits in progress.
	RelayDeposits {
		future: JoinAll<Vec<Timeout<ApiCall<H256, T::Out>>>>,
		block: u64,
	},
	/// All deposits till given block has been relayed.
	Yield(Option<u64>),
}

pub fn create_deposit_relay<T: Transport + Clone>(app: Arc<App<T>>, init: &Database, foreign_balance: Arc<RwLock<Option<U256>>>, foreign_chain_id: u64,
												  foreign_nonce: Arc<RwLock<Option<U256>>>) -> DepositRelay<T> {
	let logs_init = api::LogStreamInit {
		after: init.checked_deposit_relay,
		request_timeout: app.config.home.request_timeout,
		poll_interval: app.config.home.poll_interval,
		confirmations: app.config.home.required_confirmations,
		filter: deposits_filter(&app.home_bridge, init.home_contract_address),
	};
	DepositRelay {
		logs: api::log_stream(app.connections.home.clone(), app.timer.clone(), logs_init),
		foreign_contract: init.foreign_contract_address,
		state: DepositRelayState::Wait,
		app,
		foreign_balance,
		foreign_nonce,
		foreign_chain_id,
	}
}

pub struct DepositRelay<T: Transport> {
	app: Arc<App<T>>,
	logs: LogStream<T>,
	state: DepositRelayState<T>,
	foreign_contract: Address,
	foreign_balance: Arc<RwLock<Option<U256>>>,
	foreign_nonce: Arc<RwLock<Option<U256>>>,
	foreign_chain_id: u64,
}

impl<T: Transport> Stream for DepositRelay<T> {
	type Item = u64;
	type Error = Error;

	fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
		loop {
			let next_state = match self.state {
				DepositRelayState::Wait => {
					let foreign_balance = self.foreign_balance.read().unwrap();
					if foreign_balance.is_none() {
						warn!("foreign contract balance is unknown");
						return Ok(futures::Async::NotReady);
					}
					let foreign_nonce = self.foreign_nonce.read().unwrap();
					if foreign_nonce.is_none() {
						warn!("foreign nonce is unknown");
						return Ok(futures::Async::NotReady);
					}
					let item = try_stream!(self.logs.poll());
					info!("got {} new deposits to relay", item.logs.len());
					let balance_required = U256::from(self.app.config.txs.deposit_relay.gas) * U256::from(self.app.config.txs.deposit_relay.gas_price) * U256::from(item.logs.len());
					if balance_required > *foreign_balance.as_ref().unwrap() {
						return Err(ErrorKind::InsufficientFunds.into())
					}
					let deposits = item.logs
						.into_iter()
						.map(|log| deposit_relay_payload(&self.app.home_bridge, &self.app.foreign_bridge, log))
						.collect::<Result<Vec<_>>>()?
						.into_iter()
						.map(|payload| {
							let tx = Transaction {
								gas: self.app.config.txs.deposit_relay.gas.into(),
								gas_price: self.app.config.txs.deposit_relay.gas_price.into(),
								value: U256::zero(),
								data: payload.0,
								nonce: foreign_nonce.unwrap(),
								action: Action::Call(self.foreign_contract.clone()),
							};
							prepare_raw_transaction(tx, &self.app, &self.app.config.foreign, self.foreign_chain_id)
						})
						.map_results(|tx| {
							self.app.timer.timeout(
								api::send_raw_transaction(&self.app.connections.foreign, tx),
								self.app.config.foreign.request_timeout)
						})
						.fold_results(vec![], |mut acc, tx| {
							acc.push(tx);
							acc
						})?;

					info!("relaying {} deposits", deposits.len());
					DepositRelayState::RelayDeposits {
						future: join_all(deposits),
						block: item.to,
					}
				},
				DepositRelayState::RelayDeposits { ref mut future, block } => {
					let _ = try_ready!(future.poll());
					info!("deposit relay completed");
					DepositRelayState::Yield(Some(block))
				},
				DepositRelayState::Yield(ref mut block) => match block.take() {
					None => DepositRelayState::Wait,
					some => return Ok(some.into()),
				}
			};
			self.state = next_state;
		}
	}
}

#[cfg(test)]
mod tests {
	use rustc_hex::FromHex;
	use web3::types::{Log, Bytes};
	use contracts::{home, foreign};
	use super::deposit_relay_payload;

	#[test]
	fn test_deposit_relay_payload() {
		let home = home::HomeBridge::default();
		let foreign = foreign::ForeignBridge::default();

		let data = "000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0".from_hex().unwrap();
		let log = Log {
			data: data.into(),
			topics: vec!["e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c".into()],
			transaction_hash: Some("884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".into()),
			..Default::default()
		};

		let payload = deposit_relay_payload(&home, &foreign, log).unwrap();
		let expected: Bytes = "26b3293f000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".from_hex().unwrap().into();
		assert_eq!(expected, payload);
	}
}
