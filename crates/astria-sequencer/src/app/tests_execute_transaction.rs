use std::{
    collections::HashSet,
    str::FromStr,
    sync::Arc,
};

use astria_core::{
    crypto::SigningKey,
    oracles::price_feed::{
        market_map::v2::{
            Market,
            MarketMap,
        },
        types::v2::CurrencyPair,
    },
    primitive::v1::{
        asset,
        Address,
        RollupId,
    },
    protocol::{
        fees::v1::FeeComponents,
        genesis::v1::GenesisAppState,
        transaction::v1::{
            action::{
                BridgeLock,
                BridgeUnlock,
                CurrencyPairsChange,
                IbcRelayerChange,
                IbcSudoChange,
                MarketsChange,
                RollupDataSubmission,
                SudoAddressChange,
                Transfer,
                ValidatorName,
                ValidatorUpdate,
            },
            Action,
            TransactionBody,
        },
    },
    sequencerblock::v1::block::Deposit,
    Protobuf as _,
};
use bytes::Bytes;
use cnidarium::{
    ArcStateDeltaExt as _,
    StateDelta,
};
use futures::StreamExt as _;
use indexmap::IndexMap;

use super::test_utils::get_alice_signing_key;
use crate::{
    accounts::StateReadExt as _,
    action_handler::{
        impls::transaction::InvalidChainId,
        ActionHandler as _,
    },
    app::{
        benchmark_and_test_utils::{
            AppInitializer,
            BOB_ADDRESS,
            CAROL_ADDRESS,
        },
        test_utils::{
            get_bridge_signing_key,
            run_until_aspen_applied,
        },
        App,
        InvalidNonce,
    },
    authority::StateReadExt as _,
    benchmark_and_test_utils::{
        astria_address,
        astria_address_from_hex_string,
        nria,
        verification_key,
        ASTRIA_PREFIX,
    },
    bridge::{
        StateReadExt as _,
        StateWriteExt as _,
    },
    fees::{
        StateReadExt as _,
        StateWriteExt as _,
    },
    ibc::StateReadExt as _,
    oracles::price_feed::{
        market_map::state_ext::{
            StateReadExt as _,
            StateWriteExt as _,
        },
        oracle::state_ext::StateReadExt,
    },
    test_utils::{
        calculate_rollup_data_submission_fee_from_state,
        example_ticker_from_currency_pair,
        example_ticker_with_metadata,
    },
    utils::create_deposit_event,
};

fn proto_genesis_state() -> astria_core::generated::astria::protocol::genesis::v1::GenesisAppState {
    astria_core::generated::astria::protocol::genesis::v1::GenesisAppState {
        authority_sudo_address: Some(
            Address::builder()
                .prefix(ASTRIA_PREFIX)
                .array(get_alice_signing_key().address_bytes())
                .try_build()
                .unwrap()
                .to_raw(),
        ),
        ibc_sudo_address: Some(
            Address::builder()
                .prefix(ASTRIA_PREFIX)
                .array(get_alice_signing_key().address_bytes())
                .try_build()
                .unwrap()
                .to_raw(),
        ),
        ..crate::app::benchmark_and_test_utils::proto_genesis_state()
    }
}

fn genesis_state() -> GenesisAppState {
    GenesisAppState::try_from_raw(proto_genesis_state()).unwrap()
}

fn test_asset() -> asset::Denom {
    "test".parse().unwrap()
}

async fn initialize_app(genesis_state: Option<GenesisAppState>) -> App {
    let mut app_initializer = AppInitializer::new();
    if let Some(genesis_state) = genesis_state {
        app_initializer = app_initializer.with_genesis_state(genesis_state);
    }
    let (mut app, storage) = app_initializer.init().await;
    let _ = run_until_aspen_applied(&mut app, storage).await;
    app
}

#[tokio::test]
async fn app_execute_transaction_transfer() {
    let mut app = initialize_app(None).await;

    // transfer funds from Alice to Bob
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let bob_address = astria_address_from_hex_string(BOB_ADDRESS);
    let value = 333_333;
    let tx = TransactionBody::builder()
        .actions(vec![Transfer {
            to: bob_address,
            amount: value,
            asset: nria().into(),
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    assert_eq!(
        app.state
            .get_account_balance(&bob_address, &nria())
            .await
            .unwrap(),
        value + 10u128.pow(19)
    );
    let transfer_base = app
        .state
        .get_fees::<Transfer>()
        .await
        .expect("should not error fetching transfer fees")
        .expect("transfer fees should be stored")
        .base();
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19) - (value + transfer_base),
    );
    assert_eq!(app.state.get_account_nonce(&bob_address).await.unwrap(), 0);
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn app_execute_transaction_transfer_not_native_token() {
    use crate::accounts::StateWriteExt as _;

    let mut app = initialize_app(None).await;

    // create some asset to be transferred and update Alice's balance of it
    let value = 333_333;
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut state_tx = StateDelta::new(app.state.clone());
    state_tx
        .put_account_balance(&alice_address, &test_asset(), value)
        .unwrap();
    app.apply(state_tx);

    // transfer funds from Alice to Bob; use native token for fee payment
    let bob_address = astria_address_from_hex_string(BOB_ADDRESS);
    let tx = TransactionBody::builder()
        .actions(vec![Transfer {
            to: bob_address,
            amount: value,
            asset: test_asset(),
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    assert_eq!(
        app.state
            .get_account_balance(&bob_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19), // genesis balance
    );
    assert_eq!(
        app.state
            .get_account_balance(&bob_address, &test_asset())
            .await
            .unwrap(),
        value, // transferred amount
    );

    let transfer_base = app
        .state
        .get_fees::<Transfer>()
        .await
        .expect("should not error fetching transfer fees")
        .expect("transfer fees should be stored")
        .base();
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19) - transfer_base, // genesis balance - fee
    );
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &test_asset())
            .await
            .unwrap(),
        0, // 0 since all funds of `asset` were transferred
    );

    assert_eq!(app.state.get_account_nonce(&bob_address).await.unwrap(), 0);
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn app_execute_transaction_transfer_balance_too_low_for_fee() {
    use rand::rngs::OsRng;

    let mut app = initialize_app(None).await;

    // create a new key; will have 0 balance
    let keypair = SigningKey::new(OsRng);
    let bob = astria_address_from_hex_string(BOB_ADDRESS);

    // 0-value transfer; only fee is deducted from sender
    let tx = TransactionBody::builder()
        .actions(vec![Transfer {
            to: bob,
            amount: 0,
            asset: nria().into(),
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&keypair));
    let res = app
        .execute_transaction(signed_tx)
        .await
        .unwrap_err()
        .root_cause()
        .to_string();
    assert!(res.contains("insufficient funds"));
}

#[tokio::test]
async fn app_execute_transaction_sequence() {
    let mut app = initialize_app(None).await;
    let mut state_tx = StateDelta::new(app.state.clone());
    state_tx
        .put_fees(FeeComponents::<RollupDataSubmission>::new(0, 1))
        .unwrap();
    app.apply(state_tx);

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let data = Bytes::from_static(b"hello world");
    let fee = calculate_rollup_data_submission_fee_from_state(&data, &app.state).await;

    let tx = TransactionBody::builder()
        .actions(vec![RollupDataSubmission {
            rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
            data,
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19) - fee,
    );
}

#[tokio::test]
async fn app_execute_transaction_invalid_fee_asset() {
    let mut app = initialize_app(None).await;

    let alice = get_alice_signing_key();
    let data = Bytes::from_static(b"hello world");

    let tx = TransactionBody::builder()
        .actions(vec![RollupDataSubmission {
            rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
            data,
            fee_asset: test_asset(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    assert!(app.execute_transaction(signed_tx).await.is_err());
}

#[tokio::test]
async fn app_execute_transaction_validator_update() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(Some(genesis_state())).await;

    let update = ValidatorUpdate {
        name: ValidatorName::empty(),
        power: 100,
        verification_key: verification_key(1),
    };

    let tx = TransactionBody::builder()
        .actions(vec![Action::ValidatorUpdate(update.clone())])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    let validator_updates = app.state.get_block_validator_updates().await.unwrap();
    assert_eq!(validator_updates.len(), 1);
    assert_eq!(
        validator_updates.get(verification_key(1).address_bytes()),
        Some(&update)
    );
}

#[tokio::test]
async fn app_execute_transaction_ibc_relayer_change_addition() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(Some(genesis_state())).await;

    let tx = TransactionBody::builder()
        .actions(vec![Action::IbcRelayerChange(IbcRelayerChange::Addition(
            alice_address,
        ))])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
    assert!(app.state.is_ibc_relayer(alice_address).await.unwrap());
}

#[tokio::test]
async fn app_execute_transaction_ibc_relayer_change_deletion() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let genesis_state = {
        let mut state = proto_genesis_state();
        state.ibc_relayer_addresses.push(alice_address.to_raw());
        state
    }
    .try_into()
    .unwrap();
    let mut app = initialize_app(Some(genesis_state)).await;

    let tx = TransactionBody::builder()
        .actions(vec![IbcRelayerChange::Removal(alice_address).into()])
        .chain_id("test")
        .try_build()
        .unwrap();
    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
    assert!(!app.state.is_ibc_relayer(alice_address).await.unwrap());
}

#[tokio::test]
async fn app_execute_transaction_ibc_relayer_change_invalid() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let genesis_state = {
        let mut state = proto_genesis_state();
        state
            .ibc_sudo_address
            .replace(astria_address(&[0; 20]).to_raw());
        state.ibc_relayer_addresses.push(alice_address.to_raw());
        state
    }
    .try_into()
    .unwrap();
    let mut app = initialize_app(Some(genesis_state)).await;

    let tx = TransactionBody::builder()
        .actions(vec![IbcRelayerChange::Removal(alice_address).into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    assert!(app.execute_transaction(signed_tx).await.is_err());
}

#[tokio::test]
async fn app_execute_transaction_sudo_address_change() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(Some(genesis_state())).await;

    let new_address = astria_address_from_hex_string(BOB_ADDRESS);
    let tx = TransactionBody::builder()
        .actions(vec![Action::SudoAddressChange(SudoAddressChange {
            new_address,
        })])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    let sudo_address = app.state.get_sudo_address().await.unwrap();
    assert_eq!(sudo_address, new_address.bytes());
}

#[tokio::test]
async fn app_execute_transaction_sudo_address_change_error() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let authority_sudo_address = astria_address_from_hex_string(CAROL_ADDRESS);

    let genesis_state = {
        let mut state = proto_genesis_state();
        state
            .authority_sudo_address
            .replace(authority_sudo_address.to_raw());
        state
            .ibc_sudo_address
            .replace(astria_address(&[0u8; 20]).to_raw());
        state
    }
    .try_into()
    .unwrap();
    let mut app = initialize_app(Some(genesis_state)).await;
    let tx = TransactionBody::builder()
        .actions(vec![Action::SudoAddressChange(SudoAddressChange {
            new_address: alice_address,
        })])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    let res = app
        .execute_transaction(signed_tx)
        .await
        .unwrap_err()
        .root_cause()
        .to_string();
    assert!(res.contains("signer is not the sudo key"));
}

#[tokio::test]
async fn app_execute_transaction_fee_asset_change_addition() {
    use astria_core::protocol::transaction::v1::action::FeeAssetChange;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(Some(genesis_state())).await;

    let tx = TransactionBody::builder()
        .actions(vec![Action::FeeAssetChange(FeeAssetChange::Addition(
            test_asset(),
        ))])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    assert!(app.state.is_allowed_fee_asset(&test_asset()).await.unwrap());
}

#[tokio::test]
async fn app_execute_transaction_fee_asset_change_removal() {
    use astria_core::protocol::transaction::v1::action::FeeAssetChange;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let genesis_state = {
        let mut state = proto_genesis_state();
        state.allowed_fee_assets.push(test_asset().to_string());
        state
    }
    .try_into()
    .unwrap();
    let mut app = initialize_app(Some(genesis_state)).await;

    let tx = TransactionBody::builder()
        .actions(vec![Action::FeeAssetChange(FeeAssetChange::Removal(
            test_asset(),
        ))])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    assert!(!app.state.is_allowed_fee_asset(&test_asset()).await.unwrap());
}

#[tokio::test]
async fn app_execute_transaction_fee_asset_change_invalid() {
    use astria_core::protocol::transaction::v1::action::FeeAssetChange;

    let alice = get_alice_signing_key();

    let mut app = initialize_app(Some(genesis_state())).await;

    let tx = TransactionBody::builder()
        .actions(vec![Action::FeeAssetChange(FeeAssetChange::Removal(
            nria().into(),
        ))])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    let res = app
        .execute_transaction(signed_tx)
        .await
        .unwrap_err()
        .root_cause()
        .to_string();
    assert!(res.contains("cannot remove last allowed fee asset"));
}

#[tokio::test]
async fn app_execute_transaction_init_bridge_account_ok() {
    use astria_core::protocol::transaction::v1::action::InitBridgeAccount;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(None).await;
    let mut state_tx = StateDelta::new(app.state.clone());
    let fee = 12; // arbitrary
    state_tx
        .put_fees(FeeComponents::<InitBridgeAccount>::new(fee, 0))
        .unwrap();
    app.apply(state_tx);

    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let action = InitBridgeAccount {
        rollup_id,
        asset: nria().into(),
        fee_asset: nria().into(),
        sudo_address: None,
        withdrawer_address: None,
    };

    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));

    let before_balance = app
        .state
        .get_account_balance(&alice_address, &nria())
        .await
        .unwrap();
    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
    assert_eq!(
        app.state
            .get_bridge_account_rollup_id(&alice_address)
            .await
            .unwrap()
            .unwrap(),
        rollup_id
    );
    assert_eq!(
        app.state
            .get_bridge_account_ibc_asset(&alice_address)
            .await
            .unwrap(),
        nria().to_ibc_prefixed(),
    );
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        before_balance - fee,
    );
}

#[tokio::test]
async fn app_execute_transaction_init_bridge_account_account_already_registered() {
    use astria_core::protocol::transaction::v1::action::InitBridgeAccount;

    let alice = get_alice_signing_key();
    let mut app = initialize_app(None).await;

    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let action = InitBridgeAccount {
        rollup_id,
        asset: nria().into(),
        fee_asset: nria().into(),
        sudo_address: None,
        withdrawer_address: None,
    };
    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    let action = InitBridgeAccount {
        rollup_id,
        asset: nria().into(),
        fee_asset: nria().into(),
        sudo_address: None,
        withdrawer_address: None,
    };

    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    assert!(app.execute_transaction(signed_tx).await.is_err());
}

#[tokio::test]
async fn app_execute_transaction_bridge_lock_action_ok() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let mut app = initialize_app(None).await;

    let bridge_address = astria_address(&[99; 20]);
    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let starting_index_of_action = 0;

    let mut state_tx = StateDelta::new(app.state.clone());
    state_tx
        .put_bridge_account_rollup_id(&bridge_address, rollup_id)
        .unwrap();
    state_tx
        .put_bridge_account_ibc_asset(&bridge_address, nria())
        .unwrap();
    app.apply(state_tx);

    let amount = 100;
    let action = BridgeLock {
        to: bridge_address,
        amount,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "nootwashere".to_string(),
    };
    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));

    let bridge_before_balance = app
        .state
        .get_account_balance(&bridge_address, &nria())
        .await
        .unwrap();

    app.execute_transaction(signed_tx.clone()).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );
    let expected_deposit = Deposit {
        bridge_address,
        rollup_id,
        amount,
        asset: nria().into(),
        destination_chain_address: "nootwashere".to_string(),
        source_transaction_id: signed_tx.id(),
        source_action_index: starting_index_of_action,
    };

    assert_eq!(
        app.state
            .get_account_balance(&bridge_address, &nria())
            .await
            .unwrap(),
        bridge_before_balance + amount
    );

    let all_deposits = app.state.get_cached_block_deposits();
    let deposits = all_deposits.get(&rollup_id).unwrap();
    assert_eq!(deposits.len(), 1);
    assert_eq!(deposits[0], expected_deposit);
}

#[tokio::test]
async fn app_execute_transaction_bridge_lock_action_invalid_for_eoa() {
    use astria_core::protocol::transaction::v1::action::BridgeLock;

    let alice = get_alice_signing_key();
    let mut app = initialize_app(None).await;

    // don't actually register this address as a bridge address
    let bridge_address = astria_address(&[99; 20]);

    let amount = 100;
    let action = BridgeLock {
        to: bridge_address,
        amount,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "nootwashere".to_string(),
    };
    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    assert!(app.execute_transaction(signed_tx).await.is_err());
}

#[tokio::test]
async fn app_execute_transaction_invalid_nonce() {
    let mut app = initialize_app(None).await;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    // create tx with invalid nonce 1
    let data = Bytes::from_static(b"hello world");

    let tx = TransactionBody::builder()
        .actions(vec![RollupDataSubmission {
            rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
            data,
            fee_asset: nria().into(),
        }
        .into()])
        .nonce(1)
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    let response = app.execute_transaction(signed_tx).await;

    // check that tx was not executed by checking nonce and balance are unchanged
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        0
    );
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19),
    );

    assert_eq!(
        response
            .unwrap_err()
            .downcast_ref::<InvalidNonce>()
            .map(|nonce_err| nonce_err.0)
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn app_execute_transaction_invalid_chain_id() {
    let mut app = initialize_app(None).await;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    // create tx with invalid nonce 1
    let data = Bytes::from_static(b"hello world");
    let tx = TransactionBody::builder()
        .actions(vec![RollupDataSubmission {
            rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
            data,
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("wrong-chain")
        .try_build()
        .unwrap();
    let signed_tx = Arc::new(tx.sign(&alice));
    let response = app.execute_transaction(signed_tx).await;

    // check that tx was not executed by checking nonce and balance are unchanged
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        0
    );
    assert_eq!(
        app.state
            .get_account_balance(&alice_address, &nria())
            .await
            .unwrap(),
        10u128.pow(19),
    );

    assert_eq!(
        response
            .unwrap_err()
            .downcast_ref::<InvalidChainId>()
            .map(|chain_id_err| &chain_id_err.0)
            .unwrap(),
        "wrong-chain"
    );
}

#[tokio::test]
async fn app_stateful_check_fails_insufficient_total_balance() {
    use rand::rngs::OsRng;

    let mut app = initialize_app(None).await;

    let alice = get_alice_signing_key();

    // create a new key; will have 0 balance
    let keypair = SigningKey::new(OsRng);
    let keypair_address = astria_address(keypair.verification_key().address_bytes());

    // figure out needed fee for a single transfer
    let data = Bytes::from_static(b"hello world");
    let fee = calculate_rollup_data_submission_fee_from_state(&data, &app.state.clone()).await;

    // transfer just enough to cover single sequence fee with data
    let signed_tx = TransactionBody::builder()
        .actions(vec![Transfer {
            to: keypair_address,
            amount: fee,
            asset: nria().into(),
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap()
        .sign(&alice);
    app.execute_transaction(Arc::new(signed_tx)).await.unwrap();

    // build double transfer exceeding balance
    let signed_tx_fail = TransactionBody::builder()
        .actions(vec![
            RollupDataSubmission {
                rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
                data: data.clone(),
                fee_asset: nria().into(),
            }
            .into(),
            RollupDataSubmission {
                rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
                data: data.clone(),
                fee_asset: nria().into(),
            }
            .into(),
        ])
        .chain_id("test")
        .try_build()
        .unwrap()
        .sign(&keypair);
    // try double, see fails stateful check
    let res = signed_tx_fail
        .check_and_execute(Arc::get_mut(&mut app.state).unwrap())
        .await
        .unwrap_err()
        .root_cause()
        .to_string();
    assert!(res.contains("insufficient funds for asset"));

    // build single transfer to see passes
    let signed_tx_pass = TransactionBody::builder()
        .actions(vec![RollupDataSubmission {
            rollup_id: RollupId::from_unhashed_bytes(b"testchainid"),
            data,
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap()
        .sign(&keypair);

    signed_tx_pass
        .check_and_execute(Arc::get_mut(&mut app.state).unwrap())
        .await
        .expect("stateful check should pass since we transferred enough to cover fee");
}

#[tokio::test]
async fn app_execute_transaction_bridge_lock_unlock_action_ok() {
    use crate::accounts::StateWriteExt as _;

    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());

    let mut app = initialize_app(None).await;
    let mut state_tx = StateDelta::new(app.state.clone());

    let bridge = get_bridge_signing_key();
    let bridge_address = astria_address(&bridge.address_bytes());
    let rollup_id: RollupId = RollupId::from_unhashed_bytes(b"testchainid");

    // give bridge eoa funds so it can pay for the
    // unlock transfer action
    let transfer_base = app
        .state
        .get_fees::<Transfer>()
        .await
        .expect("should not error fetching transfer fees")
        .expect("transfer fees should be stored")
        .base();
    state_tx
        .put_account_balance(&bridge_address, &nria(), transfer_base)
        .unwrap();

    // create bridge account
    state_tx
        .put_bridge_account_rollup_id(&bridge_address, rollup_id)
        .unwrap();
    state_tx
        .put_bridge_account_ibc_asset(&bridge_address, nria())
        .unwrap();
    state_tx
        .put_bridge_account_withdrawer_address(&bridge_address, bridge_address)
        .unwrap();
    app.apply(state_tx);

    let amount = 100;
    let action = BridgeLock {
        to: bridge_address,
        amount,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "nootwashere".to_string(),
    };
    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));

    app.execute_transaction(signed_tx).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    // see can unlock through bridge unlock
    let action = BridgeUnlock {
        to: alice_address,
        amount,
        fee_asset: nria().into(),
        memo: "{ \"msg\": \"lilywashere\" }".into(),
        bridge_address,
        rollup_block_number: 1,
        rollup_withdrawal_event_id: "id-from-rollup".to_string(),
    };

    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&bridge));
    app.execute_transaction(signed_tx)
        .await
        .expect("executing bridge unlock action should succeed");
    assert_eq!(
        app.state
            .get_account_balance(&bridge_address, &nria())
            .await
            .expect("executing bridge unlock action should succeed"),
        0,
        "bridge should've transferred out whole balance"
    );
}

#[tokio::test]
async fn app_execute_transaction_action_index_correctly_increments() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let mut app = initialize_app(None).await;

    let bridge_address = astria_address(&[99; 20]);
    let rollup_id = RollupId::from_unhashed_bytes(b"testchainid");
    let starting_index_of_action = 0;

    let mut state_tx = StateDelta::new(app.state.clone());
    state_tx
        .put_bridge_account_rollup_id(&bridge_address, rollup_id)
        .unwrap();
    state_tx
        .put_bridge_account_ibc_asset(&bridge_address, nria())
        .unwrap();
    app.apply(state_tx);

    let amount = 100;
    let action = BridgeLock {
        to: bridge_address,
        amount,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "nootwashere".to_string(),
    };

    let tx = TransactionBody::builder()
        .actions(vec![action.clone().into(), action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx.clone()).await.unwrap();
    assert_eq!(
        app.state.get_account_nonce(&alice_address).await.unwrap(),
        1
    );

    let all_deposits = app.state.get_cached_block_deposits();
    let deposits = all_deposits.get(&rollup_id).unwrap();
    assert_eq!(deposits.len(), 2);
    assert_eq!(deposits[0].source_action_index, starting_index_of_action);
    assert_eq!(
        deposits[1].source_action_index,
        starting_index_of_action + 1
    );
}

#[tokio::test]
async fn transaction_execution_records_deposit_event() {
    let mut app = initialize_app(None).await;
    let mut state_tx = app
        .state
        .try_begin_transaction()
        .expect("state Arc should be present and unique");

    let alice = get_alice_signing_key();
    let bob_address = astria_address_from_hex_string(BOB_ADDRESS);
    state_tx
        .put_bridge_account_rollup_id(&bob_address, [0; 32].into())
        .unwrap();
    state_tx.put_allowed_fee_asset(&nria()).unwrap();
    state_tx
        .put_bridge_account_ibc_asset(&bob_address, nria())
        .unwrap();

    let action = BridgeLock {
        to: bob_address,
        amount: 1,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "test_chain_address".to_string(),
    };
    let tx = TransactionBody::builder()
        .actions(vec![action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();
    let signed_tx = Arc::new(tx.sign(&alice));

    let expected_deposit = Deposit {
        bridge_address: bob_address,
        rollup_id: [0; 32].into(),
        amount: 1,
        asset: nria().into(),
        destination_chain_address: "test_chain_address".to_string(),
        source_transaction_id: signed_tx.id(),
        source_action_index: 0,
    };
    let expected_deposit_event = create_deposit_event(&expected_deposit);

    signed_tx.check_and_execute(&mut state_tx).await.unwrap();
    let events = &state_tx.apply().1;
    let event = events
        .iter()
        .find(|event| event.kind == "tx.deposit")
        .expect("should have deposit event");
    assert_eq!(*event, expected_deposit_event);
}

#[tokio::test]
async fn app_execute_transaction_ibc_sudo_change() {
    let alice = get_alice_signing_key();

    let mut app = initialize_app(Some(genesis_state())).await;

    let new_address = astria_address_from_hex_string(BOB_ADDRESS);

    let tx = TransactionBody::builder()
        .actions(vec![Action::IbcSudoChange(IbcSudoChange {
            new_address,
        })])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    let ibc_sudo_address = app.state.get_ibc_sudo_address().await.unwrap();
    assert_eq!(ibc_sudo_address, new_address.bytes());
}

#[tokio::test]
async fn app_execute_transaction_ibc_sudo_change_error() {
    let alice = get_alice_signing_key();
    let alice_address = astria_address(&alice.address_bytes());
    let authority_sudo_address = astria_address_from_hex_string(CAROL_ADDRESS);

    let genesis_state = {
        let mut state = proto_genesis_state();
        state
            .authority_sudo_address
            .replace(authority_sudo_address.to_raw());
        state
            .ibc_sudo_address
            .replace(astria_address(&[0u8; 20]).to_raw());
        state
    }
    .try_into()
    .unwrap();
    let mut app = initialize_app(Some(genesis_state)).await;

    let tx = TransactionBody::builder()
        .actions(vec![Action::IbcSudoChange(IbcSudoChange {
            new_address: alice_address,
        })])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    let res = app
        .execute_transaction(signed_tx)
        .await
        .unwrap_err()
        .root_cause()
        .to_string();
    assert!(res.contains("signer is not the sudo key"));
}

#[tokio::test]
async fn transaction_execution_records_fee_event() {
    let mut app = initialize_app(None).await;

    // transfer funds from Alice to Bob
    let alice = get_alice_signing_key();
    let bob_address = astria_address_from_hex_string(BOB_ADDRESS);
    let value = 333_333;
    let tx = TransactionBody::builder()
        .actions(vec![Transfer {
            to: bob_address,
            amount: value,
            asset: nria().into(),
            fee_asset: nria().into(),
        }
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();
    let signed_tx = Arc::new(tx.sign(&alice));
    let events = app.execute_transaction(signed_tx).await.unwrap();

    let event = events.first().unwrap();
    assert_eq!(event.kind, "tx.fees");
    assert_eq!(event.attributes[0].key_bytes(), b"actionName");
    assert_eq!(event.attributes[1].key_bytes(), b"asset");
    assert_eq!(event.attributes[2].key_bytes(), b"feeAmount");
    assert_eq!(event.attributes[3].key_bytes(), b"positionInTransaction");
}

#[tokio::test]
async fn ensure_all_event_attributes_are_indexed() {
    let mut app = initialize_app(None).await;
    let mut state_tx = StateDelta::new(app.state.clone());

    let alice = get_alice_signing_key();
    let bob_address = astria_address_from_hex_string(BOB_ADDRESS);
    let value = 333_333;
    state_tx
        .put_bridge_account_rollup_id(&bob_address, [0; 32].into())
        .unwrap();
    state_tx.put_allowed_fee_asset(&nria()).unwrap();
    state_tx
        .put_bridge_account_ibc_asset(&bob_address, nria())
        .unwrap();
    app.apply(state_tx);

    let transfer_action = Transfer {
        to: bob_address,
        amount: value,
        asset: nria().into(),
        fee_asset: nria().into(),
    };
    let bridge_lock_action = BridgeLock {
        to: bob_address,
        amount: 1,
        asset: nria().into(),
        fee_asset: nria().into(),
        destination_chain_address: "test_chain_address".to_string(),
    };
    let tx = TransactionBody::builder()
        .actions(vec![transfer_action.into(), bridge_lock_action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    let events = app.execute_transaction(signed_tx).await.unwrap();

    events
        .iter()
        .flat_map(|event| &event.attributes)
        .for_each(|attribute| {
            assert!(
                attribute.index(),
                "attribute {} is not indexed",
                String::from_utf8_lossy(attribute.key_bytes()),
            );
        });
}

#[tokio::test]
async fn test_app_execute_transaction_add_and_remove_currency_pairs() {
    let alice = get_alice_signing_key();

    let mut app = initialize_app(Some(genesis_state())).await;

    // The default test genesis state contains two currency pairs: BTC/USD and ETH/USD. We'll use a
    // different one:
    let currency_pair = CurrencyPair::from_str("TIA/USD").unwrap();
    let default_currency_pairs = app
        .state
        .currency_pairs()
        .map(Result::unwrap)
        .collect::<HashSet<_>>()
        .await;
    assert!(!default_currency_pairs.contains(&currency_pair));

    let tx = TransactionBody::builder()
        .actions(vec![CurrencyPairsChange::Addition(vec![
            currency_pair.clone()
        ])
        .into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    let currency_pairs = app
        .state
        .currency_pairs()
        .map(Result::unwrap)
        .collect::<HashSet<_>>()
        .await;
    assert_eq!(currency_pairs.len(), default_currency_pairs.len() + 1);
    assert!(currency_pairs.contains(&currency_pair));

    let tx = TransactionBody::builder()
        .actions(vec![CurrencyPairsChange::Removal(vec![
            currency_pair.clone()
        ])
        .into()])
        .chain_id("test")
        .nonce(1)
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice));
    app.execute_transaction(signed_tx).await.unwrap();

    let currency_pairs = app
        .state
        .currency_pairs()
        .map(Result::unwrap)
        .collect::<HashSet<_>>()
        .await;
    assert_eq!(currency_pairs, default_currency_pairs);
}

#[tokio::test]
async fn create_markets_executes_as_expected() {
    let mut app = initialize_app(Some(genesis_state())).await;

    let ticker_1 = example_ticker_from_currency_pair("USDC", "BTC", "create market 1".to_string());
    let market_1 = Market {
        ticker: ticker_1.clone(),
        provider_configs: vec![],
    };

    let ticker_2 = example_ticker_from_currency_pair("USDC", "TIA", "create market 2".to_string());
    let market_2 = Market {
        ticker: ticker_2.clone(),
        provider_configs: vec![],
    };

    // Assert these pairs don't exist in the current market map.
    let market_map_before = app.state.get_market_map().await.unwrap().unwrap();
    assert!(!market_map_before
        .markets
        .contains_key(&ticker_1.currency_pair.to_string()));
    assert!(!market_map_before
        .markets
        .contains_key(&ticker_2.currency_pair.to_string()));

    let create_markets_action = MarketsChange::Creation(vec![market_1.clone(), market_2.clone()]);

    let tx = TransactionBody::builder()
        .actions(vec![create_markets_action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&get_alice_signing_key()));
    app.execute_transaction(signed_tx).await.unwrap();

    let market_map = app.state.get_market_map().await.unwrap().unwrap();
    assert_eq!(
        market_map.markets.len(),
        market_map_before.markets.len() + 2
    );
    assert_eq!(
        market_map.markets.get(&ticker_1.currency_pair.to_string()),
        Some(&market_1)
    );
    assert_eq!(
        market_map.markets.get(&ticker_2.currency_pair.to_string()),
        Some(&market_2)
    );
}

#[tokio::test]
async fn update_markets_executes_as_expected() {
    let mut app = initialize_app(Some(genesis_state())).await;
    // let height = run_until_aspen_applied(&mut app, storage.clone()).await;
    let mut state_tx = StateDelta::new(app.state.clone());

    let alice_signing_key = get_alice_signing_key();

    let ticker_1 = example_ticker_with_metadata("create market 1".to_string());
    let market_1 = Market {
        ticker: ticker_1.clone(),
        provider_configs: vec![],
    };
    let ticker_2 = example_ticker_from_currency_pair("USDC", "TIA", "create market 2".to_string());
    let market_2 = Market {
        ticker: ticker_2.clone(),
        provider_configs: vec![],
    };

    let mut market_map = MarketMap {
        markets: IndexMap::new(),
    };

    market_map
        .markets
        .insert(ticker_1.currency_pair.to_string(), market_1);
    market_map
        .markets
        .insert(ticker_2.currency_pair.to_string(), market_2.clone());

    state_tx.put_market_map(market_map).unwrap();
    app.apply(state_tx);

    // market_3 should replace market_1, since they share the same currency pair
    let ticker_3 = example_ticker_with_metadata("update market 1 to market 2".to_string());
    let market_3 = Market {
        ticker: ticker_3.clone(),
        provider_configs: vec![],
    };

    let update_markets_action = MarketsChange::Update(vec![market_3.clone()]);

    let tx = TransactionBody::builder()
        .actions(vec![update_markets_action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice_signing_key));
    app.execute_transaction(signed_tx).await.unwrap();

    let market_map = app.state.get_market_map().await.unwrap().unwrap();
    assert_eq!(market_map.markets.len(), 2);
    assert_eq!(
        market_map.markets.get(&ticker_1.currency_pair.to_string()),
        Some(&market_3)
    );
    assert_eq!(
        market_map.markets.get(&ticker_2.currency_pair.to_string()),
        Some(&market_2)
    );
    assert_eq!(
        market_map.markets.get(&ticker_3.currency_pair.to_string()),
        Some(&market_3)
    );
}

#[tokio::test]
async fn remove_markets_executes_as_expected() {
    let mut app = initialize_app(Some(genesis_state())).await;
    let mut state_tx = StateDelta::new(app.state.clone());

    let alice_signing_key = get_alice_signing_key();

    let ticker_1 = example_ticker_with_metadata("create market 1".to_string());
    let market_1 = Market {
        ticker: ticker_1.clone(),
        provider_configs: vec![],
    };
    let ticker_2 = example_ticker_from_currency_pair("USDC", "TIA", "create market 2".to_string());
    let market_2 = Market {
        ticker: ticker_2.clone(),
        provider_configs: vec![],
    };

    let mut market_map = MarketMap {
        markets: IndexMap::new(),
    };

    market_map
        .markets
        .insert(ticker_1.currency_pair.to_string(), market_1);
    market_map
        .markets
        .insert(ticker_2.currency_pair.to_string(), market_2.clone());

    state_tx.put_market_map(market_map).unwrap();
    app.apply(state_tx);

    let remove_markets_action = MarketsChange::Removal(vec![Market {
        ticker: ticker_1.clone(),
        provider_configs: vec![],
    }]);

    let tx = TransactionBody::builder()
        .actions(vec![remove_markets_action.into()])
        .chain_id("test")
        .try_build()
        .unwrap();

    let signed_tx = Arc::new(tx.sign(&alice_signing_key));
    app.execute_transaction(signed_tx).await.unwrap();

    let market_map = app.state.get_market_map().await.unwrap().unwrap();
    assert_eq!(market_map.markets.len(), 1);
    assert!(market_map
        .markets
        .get(&ticker_1.currency_pair.to_string())
        .is_none());
    assert_eq!(
        market_map.markets.get(&ticker_2.currency_pair.to_string()),
        Some(&market_2)
    );
}
