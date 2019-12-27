// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0
use crate::chain_watcher::{Interest, TransactionWithInfo};
use crate::channel::{
    access_local, AccessingResource, ApplyPendingTxn, ApplyTravelTxn, CancelPendingTxn, Channel,
    ChannelEvent, CollectProposalWithSigs, Execute, ForceTravel, GetPendingTxn, GrantProposal,
};
use crate::channel_state_view::ChannelStateView;
use crate::utils::contract::{
    challenge_channel_action, close_channel_action, resolve_channel_action,
};
use crate::wallet::{
    execute_transaction, submit_transaction, txn_expiration, watch_transaction, GAS_UNIT_PRICE,
    MAX_GAS_AMOUNT_OFFCHAIN, MAX_GAS_AMOUNT_ONCHAIN,
};
use anyhow::{bail, ensure, format_err, Result};
use async_trait::async_trait;
use coerce_rt::actor::{
    context::{ActorHandlerContext, ActorStatus},
    message::{Handler, Message},
    Actor, ActorRef,
};
use futures::{channel::oneshot, SinkExt, StreamExt};
use libra_crypto::ed25519::{Ed25519PublicKey, Ed25519Signature};
use libra_crypto::{hash::CryptoHash, SigningKey, VerifyingKey};
use libra_logger::prelude::*;
use libra_state_view::StateView;
use libra_types::access_path::DataPath;
use libra_types::channel::{
    make_resource, ChannelChallengeBy, ChannelLockedBy, ChannelMirrorResource,
    ChannelParticipantAccountResource, ChannelResource, LibraResource, Witness, WitnessData,
};
use libra_types::contract_event::ContractEvent;
use libra_types::identifier::Identifier;
use libra_types::language_storage::ModuleId;
use libra_types::transaction::{
    ChannelTransactionPayload, ChannelTransactionPayloadBody, RawTransaction, ScriptAction,
    SignedTransaction, Transaction, TransactionArgument, TransactionInfo, TransactionPayload,
    TransactionWithProof, Version,
};
use libra_types::write_set::WriteSet;
use libra_types::{
    access_path::AccessPath, account_address::AccountAddress, transaction::TransactionOutput,
};
use serde::de::DeserializeOwned;
use sgtypes::applied_channel_txn::AppliedChannelTxn;
use sgtypes::channel::ChannelState;
use sgtypes::channel_transaction::{ChannelOp, ChannelTransaction, ChannelTransactionProposal};
use sgtypes::channel_transaction_sigs::ChannelTransactionSigs;
use sgtypes::channel_transaction_to_commit::ChannelTransactionToCommit;
use sgtypes::pending_txn::PendingTransaction;
use sgtypes::signed_channel_transaction::SignedChannelTransaction;
use sgtypes::signed_channel_transaction_with_proof::SignedChannelTransactionWithProof;
use std::collections::BTreeMap;
use std::time::Duration;
use vm::gas_schedule::GasAlgebra;

#[async_trait]
impl Actor for Channel {
    async fn started(&mut self, ctx: &mut ActorHandlerContext) {
        let to_be_apply = {
            let pending_proposal = self.pending_txn();
            if let Some(pending_proposal) = pending_proposal {
                if pending_proposal.consensus_reached() {
                    let (proposal, _output, _) = pending_proposal.into();
                    //        debug_assert!(output.is_travel_txn(), "only travel txn is persisted");
                    Some(proposal)
                } else {
                    None
                }
            } else {
                None
            }
        };

        let mut myself = self.actor_ref(ctx).await;

        if let Some(proposal) = to_be_apply {
            if let Err(_) = myself.notify(ApplyPendingTxn { proposal }).await {
                panic!("should not happen");
            }
        }
        let mut channel_txn_receiver = self
            .chain_txn_watcher
            .add_interest(
                self.channel_address.to_vec(),
                channel_txn_interest(self.channel_address),
            )
            .await
            .unwrap();

        tokio::task::spawn(async move {
            while let Some(channel_txn) = channel_txn_receiver.next().await {
                let txn = channel_txn.clone();
                match myself.send(ApplyTravelTxn { channel_txn }).await {
                    Err(_) => {
                        info!("parent {:?} is gone, stop now", &myself);
                        break;
                    }
                    Ok(Err(e)) => {
                        error!("fail to handle travel txn: {:?}, e: {}", &txn, e);
                    }
                    _ => {}
                }
            }
        });
    }

    async fn stopped(&mut self, _ctx: &mut ActorHandlerContext) {
        if let Err(e) = self
            .channel_event_sender
            .send(ChannelEvent::Stopped {
                channel_address: self.channel_address,
            })
            .await
        {
            error!(
                "channel[{:?}]: fail to emit stopped event, error: {:?}",
                &self.channel_address, e
            );
        }
        crit!("channel {} task terminated", self.channel_address);
    }
}
#[async_trait]
impl Handler<Execute> for Channel {
    async fn handle(
        &mut self,
        message: Execute,
        _ctx: &mut ActorHandlerContext,
    ) -> <Execute as Message>::Result {
        let Execute { channel_op, args } = message;
        // generate proposal
        let proposal = self.generate_proposal(channel_op, args)?;

        // execute proposal to get txn payload and txn witness data for later use
        let (_payload_body, _payload_body_signature, output) = self.execute_proposal(&proposal)?;

        self.do_grant_proposal(proposal.clone(), output.clone(), BTreeMap::new())?;

        let pending = self.pending_txn().expect("pending txn must exists");
        let user_sigs = pending
            .get_signature(&self.account_address)
            .expect("user signature must exists");
        Ok((proposal, user_sigs, output))
    }
}

/// handle incoming proposal, return my sigs.
/// If I don't agree the proposal, return None.
/// If the proposal is already handled, also return my sigs from local cached state.
#[async_trait]
impl Handler<CollectProposalWithSigs> for Channel {
    async fn handle(
        &mut self,
        message: CollectProposalWithSigs,
        _ctx: &mut ActorHandlerContext,
    ) -> <CollectProposalWithSigs as Message>::Result {
        let CollectProposalWithSigs { proposal, sigs } = message;
        debug_assert_ne!(self.account_address, sigs.address);

        // if found an already applied txn in local storage,
        // we can return directly after check the hash of transaction and signatures.
        if let Some(signed_txn) =
            self.check_applied(proposal.channel_txn.channel_sequence_number())?
        {
            let version = signed_txn.version;
            let applied_txn = signed_txn.signed_transaction;
            match applied_txn {
                AppliedChannelTxn::Travel(_) => bail!("proposal is already commited onchain"),
                AppliedChannelTxn::Offchain(mut t) => {
                    if CryptoHash::hash(&proposal.channel_txn) != CryptoHash::hash(&t.raw_tx) {
                        bail!("invalid proposal, channel already applied a different proposal with same channel seq number {}", version);
                    }

                    let signature = t
                        .signatures
                        .remove(&self.account_address)
                        .expect("applied txn should have user signature");
                    return Ok(Some(signature));
                }
            }
        }

        self.verify_proposal(&proposal)?;

        let mut verified_signatures = BTreeMap::new();
        let (payload_body, _payload_body_signature, output) = match self.pending_txn() {
            None => self.execute_proposal(&proposal)?,
            Some(p) if p.is_negotiating() => {
                let (local_proposal, output, signatures) = p.into();

                ensure!(
                    CryptoHash::hash(&proposal.channel_txn)
                        == CryptoHash::hash(&local_proposal.channel_txn),
                    format_err!("channel txn conflict with local")
                );
                ensure!(
                    &proposal.proposer_public_key == &local_proposal.proposer_public_key,
                    format_err!("txn proposer public_key conflict with local")
                );
                debug_assert_eq!(
                    &local_proposal.proposer_signature,
                    &proposal.proposer_signature
                );
                verified_signatures = signatures;

                let (payload_body, payload_body_signature) =
                    self.build_and_sign_channel_txn_payload_body(&proposal.channel_txn)?;
                (payload_body, payload_body_signature, output)
            }
            Some(p) => match p.get_signature(&self.account_address) {
                Some(s) => return Ok(Some(s)),
                None => {
                    panic!("should already give out user signature");
                }
            },
        };

        self.verify_txn_sigs(&payload_body, &output, &sigs)?;

        verified_signatures.insert(sigs.address, sigs);

        // if the output modifies user's channel state, permission need to be granted by user.
        // it cannot be auto-signed.
        let can_auto_signed = !output
            .write_set()
            .contains_channel_resource(&self.account_address);
        if !verified_signatures.contains_key(&self.account_address) && can_auto_signed {
            self.do_grant_proposal(proposal, output, verified_signatures)?;
        } else {
            self.save_pending_txn(proposal, output, verified_signatures)?;
        };

        let pending = self.pending_txn().expect("pending txn must exists");
        let user_sigs = pending.get_signature(&self.account_address);
        Ok(user_sigs)
    }
}

#[async_trait]
impl Handler<GrantProposal> for Channel {
    async fn handle(
        &mut self,
        message: GrantProposal,
        ctx: &mut ActorHandlerContext,
    ) -> <GrantProposal as Message>::Result {
        let GrantProposal {
            channel_txn_id,
            grant,
        } = message;
        let pending_txn = self.pending_txn();
        ensure!(pending_txn.is_some(), "no pending txn");
        let pending_txn = pending_txn.unwrap();
        ensure!(
            !pending_txn.consensus_reached(),
            "pending txn is already consensus_reached"
        );
        let (proposal, output, signatures) = pending_txn.into();
        if channel_txn_id != CryptoHash::hash(&proposal.channel_txn) {
            let err = format_err!("channel_txn_id conflict with local pending txn");
            return Err(err);
        }
        if grant {
            // maybe already grant the proposal
            if !signatures.contains_key(&self.account_address) {
                self.do_grant_proposal(proposal, output, signatures)?;
            }
            let pending = self.pending_txn().expect("pending txn must exists");
            let user_sigs = pending
                .get_signature(&self.account_address)
                .expect("user signature must exists");
            Ok(Some(user_sigs))
        } else {
            self.clear_pending_txn()?;
            if proposal.channel_txn.operator().is_open() {
                ctx.set_status(ActorStatus::Stopping);
            }
            Ok(None)
        }
    }
}

#[async_trait]
impl Handler<CancelPendingTxn> for Channel {
    async fn handle(
        &mut self,
        message: CancelPendingTxn,
        ctx: &mut ActorHandlerContext,
    ) -> <CancelPendingTxn as Message>::Result {
        let CancelPendingTxn { channel_txn_id } = message;

        let pending_txn = self.pending_txn();
        ensure!(pending_txn.is_some(), "no pending txn");
        let pending_txn = pending_txn.unwrap();
        ensure!(
            !pending_txn.consensus_reached(),
            "pending txn is already consensus_reached"
        );
        let (proposal, _output, _signature) = pending_txn.into();
        if channel_txn_id != CryptoHash::hash(&proposal.channel_txn) {
            let err = format_err!("channel_txn_id conflict with local pending txn");
            return Err(err);
        }
        self.clear_pending_txn()?;

        if proposal.channel_txn.operator().is_open() {
            ctx.set_status(ActorStatus::Stopping);
        }
        Ok(())
    }
}

#[async_trait]
impl Handler<ApplyPendingTxn> for Channel {
    async fn handle(
        &mut self,
        message: ApplyPendingTxn,
        _ctx: &mut ActorHandlerContext,
    ) -> <ApplyPendingTxn as Message>::Result {
        let ApplyPendingTxn { proposal } = message;

        if let Some(_signed_txn) =
            self.check_applied(proposal.channel_txn.channel_sequence_number())?
        {
            warn!(
                "txn {} already applied!",
                &CryptoHash::hash(&proposal.channel_txn)
            );
            //            let res = Ok::<u64, Error>(if signed_txn.proof.transaction_info().travel() {
            //                signed_txn.proof.transaction_info().gas_used()
            //            } else {
            //                0
            //            });
            //
            return Ok(None);
        }

        debug!("user {} apply txn", self.account_address);
        ensure!(self.pending_txn().is_some(), "should have txn to apply");
        let pending_txn = self.pending_txn().unwrap();
        ensure!(
            pending_txn.consensus_reached(),
            "txn should have been consensus_reached"
        );
        let (proposal, output, signatures) = pending_txn.into();

        if !output.is_travel_txn() {
            self.apply(proposal.channel_txn, output, signatures)?;
            return Ok(None);
        }

        let channel_txn = &proposal.channel_txn;
        if self.account_address == channel_txn.proposer() {
            let max_gas_amount = std::cmp::min(
                (output.gas_used() as f64 * 1.1) as u64,
                MAX_GAS_AMOUNT_ONCHAIN,
            );
            let (payload_body, _) = self.build_and_sign_channel_txn_payload_body(channel_txn)?;

            let signed_txn = self.build_raw_txn_from_channel_txn(
                payload_body,
                &channel_txn,
                Some(&signatures),
                max_gas_amount,
            )?;

            submit_transaction(self.chain_client.as_ref(), signed_txn).await?;
        }

        let txn_sender = channel_txn.proposer();
        let seq_number = channel_txn.sequence_number();
        // savce proposal as applying
        let mut pending_proposal = PendingTransaction::new(proposal, output, signatures);
        pending_proposal.set_applying();
        self.store.save_pending_txn(pending_proposal, true)?;

        Ok(Some((txn_sender, seq_number)))
        //        self.watch_and_travel_txn_async(ctx, txn_sender, seq_number)
        //            .await
    }
}

#[async_trait]
impl Handler<ForceTravel> for Channel {
    async fn handle(
        &mut self,
        _message: ForceTravel,
        _ctx: &mut ActorHandlerContext,
    ) -> <ForceTravel as Message>::Result {
        debug!("user {} apply txn", self.account_address);
        ensure!(self.pending_txn().is_some(), "should have txn to apply");
        let pending_txn = self.pending_txn().unwrap();
        ensure!(
            !pending_txn.consensus_reached(),
            "txn should not be consensus_reached"
        );
        let (proposal, output, _signatures) = pending_txn.into();
        let channel_txn = &proposal.channel_txn;
        ensure!(
            self.account_address == channel_txn.proposer(),
            "solo should only use myself's proposal"
        );

        let max_gas_amount = std::cmp::min(
            (output.gas_used() as f64 * 1.1) as u64,
            MAX_GAS_AMOUNT_ONCHAIN,
        );
        let action =
            self.channel_op_to_action(channel_txn.operator(), channel_txn.args().to_vec())?;
        let signed_txn = self.build_solo_txn(action, max_gas_amount)?;
        submit_transaction(self.chain_client.as_ref(), signed_txn.clone()).await?;

        let txn_sender = signed_txn.sender();
        let seq_number = signed_txn.sequence_number();

        Ok((txn_sender, seq_number))
    }
}

#[async_trait]
impl Handler<AccessingResource> for Channel {
    async fn handle(
        &mut self,
        message: AccessingResource,
        _ctx: &mut ActorHandlerContext,
    ) -> <AccessingResource as Message>::Result {
        let AccessingResource { path } = message;
        access_local(self.witness_data().write_set(), &self.channel_state, &path)
            .map(|d| d.map(|o| o.to_vec()))
    }
}

#[async_trait]
impl Handler<GetPendingTxn> for Channel {
    async fn handle(
        &mut self,
        _message: GetPendingTxn,
        _ctx: &mut ActorHandlerContext,
    ) -> <GetPendingTxn as Message>::Result {
        self.pending_txn()
    }
}

/// when a new txn is included on chain(no matter who proposed it), node should `ApplyTravelTxn`,
/// It should also handle un-authorized txn.  
#[async_trait]
impl Handler<ApplyTravelTxn> for Channel {
    async fn handle(
        &mut self,
        message: ApplyTravelTxn,
        _ctx: &mut ActorHandlerContext,
    ) -> <ApplyTravelTxn as Message>::Result {
        let ApplyTravelTxn {
            channel_txn:
                TransactionWithInfo {
                    txn,
                    txn_info,
                    version,
                    events,
                    ..
                },
        } = message;
        let signed_txn = match txn {
            Transaction::UserTransaction(s) => s,
            _ => {
                bail!("should be user txn");
            }
        };

        let raw_txn = signed_txn.raw_txn();
        let txn_sender = raw_txn.sender();
        let channel_txn_payload = match raw_txn.payload() {
            TransactionPayload::Channel(channel_txn_payload) => channel_txn_payload,
            _ => bail!("should be channel txn"),
        };

        debug_assert!(self.participant_addresses.contains(&txn_sender));
        debug_assert!(self.channel_address == channel_txn_payload.channel_address());
        let channel_txn_proposer = channel_txn_payload.proposer();
        let txn_channel_seq_number = channel_txn_payload.witness().channel_sequence_number();
        let local_channel_seq_number = self.channel_sequence_number();
        // compare the new txn's witness sequence number with local sequence_number
        // if equal, it means new txn committed on-chain, but I don't aware.
        // if less by only one, maybe proposer didn't receive my signature, and he proposed the txn on-chain,
        // or it means the new txn proposer had submitted a stale channel txn purposely.
        // if bigger, it's a bug.
        debug_assert!(
            txn_channel_seq_number <= local_channel_seq_number,
            "Local state is stale, there must be some bugs"
        );

        // If the txn is sent by myself.
        if self.account_address == channel_txn_proposer {
            // if the message is outdated
            if channel_txn_payload.channel_sequence_number() < self.channel_sequence_number() {
                let applied_txn =
                    self.check_applied(channel_txn_payload.channel_sequence_number())?;
                debug_assert!(applied_txn.is_some());
                let applied_txn = applied_txn.unwrap();
                debug_assert!(applied_txn.signed_transaction.travel());
                // TODO: should check txn hash?
                return Ok(applied_txn.proof.transaction_info().gas_used());
            }

            debug_assert!(
                channel_txn_payload.channel_sequence_number() == self.channel_sequence_number()
            );
            let gas_used = txn_info.gas_used();
            // 1. I trust the txn and apply it into local.
            self.apply_travel(version, signed_txn.clone(), txn_info, events)?;
            // 2. after apply, check channel state
            let channel_resource: ChannelResource = self
                .channel_resource()
                .ok_or(format_err!("channel resource should exists in local"))?;

            if channel_resource.locked() {
                // I lock the channel, wait sender to resolve
                // TODO: move the timeout check into a timer
                self.watch_channel_lock_timeout(_ctx).await?;
            } else if channel_resource.closed() {
                _ctx.set_status(ActorStatus::Stopping);
            } else {
                // nothing to do. everything is good now.
            }
            return Ok(gas_used);
        }

        //-- Here participant handle the new travel txn.

        if txn_channel_seq_number < local_channel_seq_number {
            if channel_txn_payload.is_authorized() {
                // a late txn is arrived
                let local_txn = self
                    .check_applied(txn_channel_seq_number)?
                    .expect("get local applied txn should be ok");
                match &local_txn.signed_transaction {
                    AppliedChannelTxn::Travel(s) if s.raw_txn().hash() == raw_txn.hash() => {
                        // it's ok, it may be a late message.
                        return Ok(local_txn.proof.transaction_info().gas_used());
                    }
                    _ => {
                        // it means participant make a stale offchain txn travel directly.
                        // but I already applied the txn locally.
                        // I cannot challenge him, because the channel is not locked.
                        // FIXME: what's should I do.
                        unimplemented!()
                    }
                }
            } else {
                // dual submits a stale txn, I need to challenge him

                // so I submit a challenge to chain.
                // and don't watch the txn.
                let _ = self
                    .solo_txn(_ctx, challenge_channel_action(), u64::max_value())
                    .await?;
                return Ok(0);
            }
        }

        debug_assert!(txn_channel_seq_number == local_channel_seq_number);

        // a newer txn is committed onchain by dual.
        // no matter whether I signed it or not, it's trusted by on-chain,
        // I also trust it, and apply it.
        let gas_used = txn_info.gas_used();
        self.apply_travel(version, signed_txn.clone(), txn_info, events)?;
        // after apply, check channel state
        let channel_resource: ChannelResource = self
            .channel_resource()
            .ok_or(format_err!("channel resource should exists in local"))?;

        // if channel is lock, it means dual submit a solo txn, I need to resolve it.
        if channel_resource.locked() {
            debug_assert!(!channel_txn_payload.is_authorized());
            // drop the receiver, as I don't need wait the result
            let _ = self
                .solo_txn(_ctx, resolve_channel_action(), u64::max_value())
                .await?;
        } else if channel_resource.closed() {
            // no matter who close ths channel, the channel is done. we just act on it.
            // TODO: close channel now
            _ctx.set_status(ActorStatus::Stopping);
        } else {
            // every thing is fine.
        }
        return Ok(gas_used);
    }
}

struct ChannelLockTimeout;
impl Message for ChannelLockTimeout {
    type Result = Result<()>;
}
#[async_trait]
impl Handler<ChannelLockTimeout> for Channel {
    async fn handle(
        &mut self,
        _message: ChannelLockTimeout,
        ctx: &mut ActorHandlerContext,
    ) -> <ChannelLockTimeout as Message>::Result {
        let _ = self
            .solo_txn(ctx, close_channel_action(), u64::max_value())
            .await?;
        Ok(())
    }
}

impl Channel {
    fn build_solo_txn(
        &self,
        action: ScriptAction,
        max_gas_amount: u64,
    ) -> Result<SignedTransaction> {
        let (body, payload_body_signature) =
            self.propose_channel_action(self.account_address, action)?;
        let mut signatures = BTreeMap::new();
        signatures.insert(
            self.account_address,
            (self.keypair.public_key.clone(), payload_body_signature),
        );

        let account_seq_number = self
            .chain_client
            .account_sequence_number(&self.account_address)
            .ok_or(format_err!("account not exists"))?;
        let txn = self.build_chain_txn(
            body,
            Some(signatures),
            self.account_address,
            account_seq_number,
            max_gas_amount,
            Duration::from_secs(60),
        )?;
        Ok(txn)
    }

    fn channel_view(&self, version: Option<Version>) -> Result<ChannelStateView> {
        let latest_writeset = self.witness_data().into_write_set();
        ChannelStateView::new(
            self.account_address,
            &self.channel_state,
            latest_writeset,
            version,
            self.chain_client.as_ref(),
        )
    }

    fn build_chain_txn(
        &self,
        channel_payload_body: ChannelTransactionPayloadBody,
        txn_signatures: Option<BTreeMap<AccountAddress, (Ed25519PublicKey, Ed25519Signature)>>,
        txn_sender: AccountAddress,
        sender_seq_number: u64,
        max_gas_amount: u64,
        expiration_time: Duration,
    ) -> Result<SignedTransaction> {
        let channel_participant_size = self.participant_addresses.len();
        let mut participant_keys = self.store.get_participant_keys();
        let mut sigs = Vec::with_capacity(channel_participant_size);
        if let Some(signatures) = txn_signatures {
            for addr in self.participant_addresses.iter() {
                let sig = signatures.get(&addr);
                if let Some(s) = sig {
                    participant_keys.insert(addr.clone(), s.0.clone());
                }
                sigs.push(sig.map(|s| s.1.clone()));
            }
        }

        if channel_payload_body.witness().channel_sequence_number() == 0 {
            //            debug_assert!(channel_txn.operator().is_open());
        } else {
            debug_assert!(channel_participant_size == participant_keys.len());
        }

        let keys = participant_keys
            .into_iter()
            .map(|p| p.1)
            .collect::<Vec<_>>();

        let channel_txn_payload = ChannelTransactionPayload::new(channel_payload_body, keys, sigs);
        let txn_payload = TransactionPayload::Channel(channel_txn_payload);

        let raw_txn = RawTransaction::new(
            txn_sender,
            sender_seq_number,
            txn_payload,
            max_gas_amount,
            GAS_UNIT_PRICE,
            expiration_time,
        );
        Ok(raw_txn
            .sign(&self.keypair.private_key, self.keypair.public_key.clone())?
            .into_inner())
    }

    fn build_raw_txn_from_channel_txn(
        &self,
        channel_payload_body: ChannelTransactionPayloadBody,
        channel_txn: &ChannelTransaction,
        txn_signatures: Option<&BTreeMap<AccountAddress, ChannelTransactionSigs>>,
        max_gas_amount: u64,
    ) -> Result<SignedTransaction> {
        let channel_payload_signatures = txn_signatures.map(|s| {
            s.into_iter()
                .map(|(k, v)| {
                    let ChannelTransactionSigs {
                        public_key,
                        channel_payload_signature,
                        ..
                    } = v;
                    (
                        k.clone(),
                        (public_key.clone(), channel_payload_signature.clone()),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        });
        self.build_chain_txn(
            channel_payload_body,
            channel_payload_signatures,
            channel_txn.proposer(),
            channel_txn.sequence_number(),
            max_gas_amount,
            channel_txn.expiration_time(),
        )
    }

    fn propose_channel_action(
        &self,
        proposer: AccountAddress,
        action: ScriptAction,
    ) -> Result<(ChannelTransactionPayloadBody, Ed25519Signature)> {
        let body = ChannelTransactionPayloadBody::new(
            self.channel_address,
            proposer,
            action,
            self.witness_data(),
        );
        let body_hash = CryptoHash::hash(&body);
        let sig = self.keypair.private_key.sign_message(&body_hash);
        Ok((body, sig))
    }

    /// build channel txn payload version 2.
    fn build_and_sign_channel_txn_payload_body(
        &self,
        channel_txn: &ChannelTransaction,
    ) -> Result<(ChannelTransactionPayloadBody, Ed25519Signature)> {
        let action =
            self.channel_op_to_action(channel_txn.operator(), channel_txn.args().to_vec())?;
        self.propose_channel_action(channel_txn.proposer(), action)
    }

    async fn watch_and_travel_txn_async(
        &mut self,
        ctx: &mut ActorHandlerContext,
        txn_sender: AccountAddress,
        seq_number: u64,
    ) -> Result<oneshot::Receiver<Result<u64>>> {
        let _i_am_sender = self.account_address == txn_sender;
        let watcher = watch_transaction(self.chain_client.clone(), txn_sender, seq_number);
        let (tx, rx) = oneshot::channel();
        let mut myself = self.actor_ref(ctx).await;
        tokio::spawn(async move {
            match watcher.await {
                Ok(TransactionWithProof {
                    version,
                    transaction,
                    events,
                    proof,
                }) => {
                    let result = myself
                        .send(ApplyTravelTxn {
                            channel_txn: TransactionWithInfo {
                                version,
                                txn: transaction,
                                txn_info: proof.transaction_info().clone(),
                                events: events.unwrap_or_default(),
                                block_id: 0, // FIXME
                            },
                        })
                        .await;
                    let result = result
                        .map_err(|_| format_err!("channel actor gone"))
                        .and_then(|r| r);
                    let _ = tx.send(result);
                }
                Err(_e) => {
                    error!("sender is dropped by chain txn watcher, that's wired");
                    let _ = tx.send(Err(format_err!("sender is dropped")));
                }
            }
        });
        Ok(rx)
    }

    /// submit solo txn
    async fn solo_txn(
        &mut self,
        ctx: &mut ActorHandlerContext,
        action: ScriptAction,
        max_gas_amount: u64,
    ) -> Result<oneshot::Receiver<Result<u64>>> {
        let signed_txn = self.build_solo_txn(action, max_gas_amount)?;
        submit_transaction(self.chain_client.as_ref(), signed_txn.clone()).await?;
        let txn_sender = signed_txn.sender();
        let seq_number = signed_txn.sequence_number();
        self.watch_and_travel_txn_async(ctx, txn_sender, seq_number)
            .await
    }

    fn verify_proposal(&self, proposal: &ChannelTransactionProposal) -> Result<()> {
        let channel_txn = &proposal.channel_txn;
        ensure!(
            self.channel_address == channel_txn.channel_address(),
            "invalid channel address"
        );
        let channel_sequence_number = self.channel_sequence_number();
        let smallest_allowed_channel_seq_number =
            channel_sequence_number.checked_sub(1).unwrap_or(0);
        ensure!(
            channel_txn.channel_sequence_number() >= smallest_allowed_channel_seq_number
                && channel_txn.channel_sequence_number() <= channel_sequence_number,
            "check channel_sequence_number fail."
        );
        proposal
            .proposer_public_key
            .verify_signature(&CryptoHash::hash(channel_txn), &proposal.proposer_signature)?;

        // TODO: check public key match proposer address
        if !channel_txn.operator().is_open() {
            ensure!(
                self.participant_addresses.contains(&channel_txn.proposer()),
                "proposer does not belong to the channel"
            );
        }
        Ok(())
    }

    fn apply_travel(
        &mut self,
        version: u64,
        signed_txn: SignedTransaction,
        txn_info: TransactionInfo,
        events: Vec<ContractEvent>,
    ) -> Result<()> {
        let txn_to_commit = ChannelTransactionToCommit {
            signed_channel_txn: AppliedChannelTxn::Travel(signed_txn),
            events,
            major_status: txn_info.major_status(),
            write_set: WriteSet::default(),
            gas_used: txn_info.gas_used(),
        };
        self.tx_applier.apply(txn_to_commit)?;

        self.refresh_channel_state(version)?;
        Ok(())
    }

    fn refresh_channel_state(&mut self, version: u64) -> Result<()> {
        let channel_address_state = self
            .chain_client
            .get_account_state(self.channel_address, Some(version))?;
        self.channel_state = ChannelState::new(self.channel_address, channel_address_state);

        Ok(())
    }

    /// apply data into local channel storage
    fn apply(
        &mut self,
        channel_txn: ChannelTransaction,
        txn_output: TransactionOutput,
        signatures: BTreeMap<AccountAddress, ChannelTransactionSigs>,
    ) -> Result<()> {
        let txn_to_commit = ChannelTransactionToCommit {
            signed_channel_txn: AppliedChannelTxn::Offchain(SignedChannelTransaction::new(
                channel_txn,
                signatures,
            )),
            events: txn_output.events().to_vec(),
            major_status: txn_output.status().vm_status().major_status,
            write_set: txn_output.write_set().clone(),
            gas_used: txn_output.gas_used(),
        };

        // apply txn  also delete pending txn from db
        self.tx_applier.apply(txn_to_commit)?;
        Ok(())
    }

    fn witness_data(&self) -> Witness {
        self.store.get_latest_witness().unwrap_or_default()
    }

    fn channel_sequence_number(&self) -> u64 {
        let channel_mirror_resource = self
            .get_local::<ChannelMirrorResource>(&AccessPath::new_for_data_path(
                self.channel_address,
                DataPath::channel_resource_path(
                    self.channel_address,
                    ChannelMirrorResource::struct_tag(),
                ),
            ))
            .unwrap();
        match channel_mirror_resource {
            None => 0,
            Some(r) => r.channel_sequence_number(),
        }
    }

    fn channel_resource(&self) -> Option<ChannelResource> {
        self.get_local::<ChannelResource>(&AccessPath::new_for_data_path(
            self.channel_address,
            DataPath::onchain_resource_path(ChannelResource::struct_tag()),
        ))
        .unwrap()
    }
    fn channel_lock_by_resource(&self) -> Option<ChannelLockedBy> {
        self.get_local::<ChannelLockedBy>(&AccessPath::new_for_data_path(
            self.channel_address,
            DataPath::onchain_resource_path(ChannelLockedBy::struct_tag()),
        ))
        .unwrap()
    }

    #[allow(dead_code)]
    fn channel_challenge_by_resource(&self) -> Option<ChannelChallengeBy> {
        self.get_local::<ChannelChallengeBy>(&AccessPath::new_for_data_path(
            self.channel_address,
            DataPath::onchain_resource_path(ChannelChallengeBy::struct_tag()),
        ))
        .unwrap()
    }

    #[allow(dead_code)]
    fn channel_account_resource(&self) -> Option<ChannelParticipantAccountResource> {
        self.get_local::<ChannelParticipantAccountResource>(&AccessPath::new_for_data_path(
            self.channel_address,
            DataPath::channel_resource_path(
                self.account_address,
                ChannelParticipantAccountResource::struct_tag(),
            ),
        ))
        .unwrap()
    }

    fn pending_txn(&self) -> Option<PendingTransaction> {
        self.store.get_pending_txn()
    }

    // TODO: should stage is needed?

    //    fn _stage(&self) -> ChannelStage {
    //        match self.pending_txn() {
    //            Some(PendingTransaction::WaitForApply { .. }) => ChannelStage::Syncing,
    //            Some(PendingTransaction::WaitForSig { .. }) => ChannelStage::Pending,
    //            None => match self.channel_account_resource() {
    //                Some(resource) => {
    //                    if resource.closed() {
    //                        ChannelStage::Closed
    //                    } else {
    //                        ChannelStage::Idle
    //                    }
    //                }
    //                None => ChannelStage::Opening,
    //            },
    //        }
    //    }
    //
    //    fn _check_stage(&self, expect_stages: Vec<ChannelStage>) -> Result<()> {
    //        let current_stage = self._stage();
    //        if !expect_stages.contains(&current_stage) {
    //            return Err(SgError::new_invalid_channel_stage_error(current_stage).into());
    //        }
    //        Ok(())
    //    }

    fn get_local<T>(&self, access_path: &AccessPath) -> Result<Option<T>>
    where
        T: LibraResource + DeserializeOwned,
    {
        let witness = self.witness_data();
        let data = access_local(witness.write_set(), &self.channel_state, access_path)?;
        data.map(make_resource).transpose()
    }

    fn generate_proposal(
        &self,
        channel_op: ChannelOp,
        args: Vec<TransactionArgument>,
    ) -> Result<ChannelTransactionProposal> {
        // TODO: state view should be shared to reduce fetching account state from layer1.
        let state_view = self.channel_view(None)?;

        // account state already cached in state view
        let account_seq_number = {
            let account_resource_blob = state_view
                .get(&AccessPath::new_for_account_resource(self.account_address))?
                .ok_or(format_err!(
                    "account resource for {} not exists on chain",
                    self.account_address,
                ))?;
            let account_resource =
                sgtypes::account_resource_ext::from_bytes(&account_resource_blob)?;
            account_resource.sequence_number()
        };

        let chain_version = state_view.version();
        // build channel_transaction first
        let channel_txn = ChannelTransaction::new(
            chain_version,
            self.channel_address,
            self.channel_sequence_number(),
            channel_op,
            args,
            self.account_address,
            account_seq_number,
            txn_expiration(),
        );
        let channel_txn_hash = CryptoHash::hash(&channel_txn);
        let channel_txn_signature = self.keypair.private_key.sign_message(&channel_txn_hash);

        let proposal = ChannelTransactionProposal {
            channel_txn,
            proposer_public_key: self.keypair.public_key.clone(),
            proposer_signature: channel_txn_signature,
        };
        Ok(proposal)
    }

    fn execute_proposal(
        &self,
        proposal: &ChannelTransactionProposal,
    ) -> Result<(
        ChannelTransactionPayloadBody,
        Ed25519Signature,
        TransactionOutput,
    )> {
        let channel_txn = &proposal.channel_txn;
        let (payload_body, payload_body_signature) =
            self.build_and_sign_channel_txn_payload_body(channel_txn)?;

        let output = {
            // create mocked txn to execute
            // execute txn on offchain vm, should mock sender and receiver signature with a local
            // keypair. the vm will skip signature check on offchain vm.
            let txn = self.build_raw_txn_from_channel_txn(
                payload_body.clone(),
                channel_txn,
                None,
                MAX_GAS_AMOUNT_OFFCHAIN,
            )?;

            let version = channel_txn.version();
            let state_view = self.channel_view(Some(version))?;
            execute_transaction(&state_view, txn)?
        };

        // check output gas
        let gas_used = output.gas_used();
        if gas_used > vm::gas_schedule::MAXIMUM_NUMBER_OF_GAS_UNITS.get() {
            warn!(
                "GasUsed {} > gas_schedule::MAXIMUM_NUMBER_OF_GAS_UNITS {}",
                gas_used,
                vm::gas_schedule::MAXIMUM_NUMBER_OF_GAS_UNITS.get()
            );
        }

        Ok((payload_body, payload_body_signature, output))
    }
    fn check_applied(
        &self,
        channel_sequence_number: u64,
    ) -> Result<Option<(SignedChannelTransactionWithProof)>> {
        if let Some(info) = self.store.get_startup_info()? {
            if channel_sequence_number > info.latest_version {
                Ok(None)
            } else {
                let signed_channel_txn_with_proof = self
                    .store
                    .get_transaction_by_channel_seq_number(channel_sequence_number, false)?;
                debug_assert_eq!(
                    signed_channel_txn_with_proof.version,
                    channel_sequence_number
                );
                Ok(Some(signed_channel_txn_with_proof))
            }
        } else {
            Ok(None)
        }
    }

    fn generate_txn_sigs(
        &self,
        channel_txn: &ChannelTransaction,
        output: &TransactionOutput,
    ) -> Result<ChannelTransactionSigs> {
        let (_, payload_body_signature) =
            self.build_and_sign_channel_txn_payload_body(channel_txn)?;

        let ws = if output.is_travel_txn() {
            WriteSet::default()
        } else {
            output.write_set().clone()
        };
        let witness_data = WitnessData::new(self.channel_sequence_number() + 1, ws);
        let witness_data_hash = CryptoHash::hash(&witness_data);
        let witness_data_signature = self.keypair.private_key.sign_message(&witness_data_hash);

        let travel_output_witness_signature = if output.is_travel_txn() {
            let txn_output_witness_data = WitnessData::new(
                self.channel_sequence_number() + 1,
                output.write_set().clone(),
            );
            Some(
                self.keypair
                    .private_key
                    .sign_message(&CryptoHash::hash(&txn_output_witness_data)),
            )
        } else {
            None
        };

        let generated_sigs = ChannelTransactionSigs::new(
            self.account_address,
            self.keypair.public_key.clone(),
            payload_body_signature,
            witness_data_hash,
            witness_data_signature,
            travel_output_witness_signature,
        );

        Ok(generated_sigs)
    }

    fn verify_txn_sigs(
        &self,
        payload_body: &ChannelTransactionPayloadBody,
        output: &TransactionOutput,
        channel_txn_sigs: &ChannelTransactionSigs,
    ) -> Result<()> {
        channel_txn_sigs.public_key.verify_signature(
            &CryptoHash::hash(payload_body),
            &channel_txn_sigs.channel_payload_signature,
        )?;

        let ws = if output.is_travel_txn() {
            WriteSet::default()
        } else {
            output.write_set().clone()
        };
        let witness_data = WitnessData::new(self.channel_sequence_number() + 1, ws);

        ensure!(
            &CryptoHash::hash(&witness_data) == &channel_txn_sigs.witness_data_hash,
            "witness hash mismatched"
        );
        channel_txn_sigs.public_key.verify_signature(
            &channel_txn_sigs.witness_data_hash,
            &channel_txn_sigs.witness_data_signature,
        )?;

        if output.is_travel_txn() {
            match &channel_txn_sigs.travel_output_witness_signature {
                None => bail!("travel txn should have signer's signature on output"),
                Some(signature) => {
                    let txn_output_witness_data = WitnessData::new(
                        self.channel_sequence_number() + 1,
                        output.write_set().clone(),
                    );
                    channel_txn_sigs
                        .public_key
                        .verify_signature(&CryptoHash::hash(&txn_output_witness_data), signature)?;
                }
            }
        }

        Ok(())
    }

    /// Grant the proposal and save it into pending txn
    fn do_grant_proposal(
        &mut self,
        proposal: ChannelTransactionProposal,
        output: TransactionOutput,
        mut signatures: BTreeMap<AccountAddress, ChannelTransactionSigs>,
    ) -> Result<()> {
        let user_sigs = self.generate_txn_sigs(&proposal.channel_txn, &output)?;
        signatures.insert(user_sigs.address, user_sigs.clone());
        debug!(
            "user {:?} add signature to txn {}",
            self.account_address,
            CryptoHash::hash(&proposal.channel_txn),
        );
        self.save_pending_txn(proposal, output, signatures)
    }

    fn save_pending_txn(
        &mut self,
        proposal: ChannelTransactionProposal,
        output: TransactionOutput,
        signatures: BTreeMap<AccountAddress, ChannelTransactionSigs>,
    ) -> Result<()> {
        let mut pending_txn = PendingTransaction::new(proposal, output, signatures);
        if pending_txn.try_reach_consensus(&self.participant_addresses) {
            info!(
                "wallet {}: consensus on channel {} is reached",
                self.account_address, self.channel_address,
            );
        }
        // always persist pending txn
        self.store.save_pending_txn(pending_txn, true)?;
        Ok(())
    }

    /// helper method to get self actor ref from `ctx`
    async fn actor_ref(&self, ctx: &mut ActorHandlerContext) -> ActorRef<Self> {
        let self_id = ctx.actor_id().clone();
        let self_ref = ctx
            .actor_context_mut()
            .get_actor::<Self>(self_id)
            .await
            .expect("get self actor ref should be ok");
        self_ref
    }
    async fn watch_channel_lock_timeout(&self, ctx: &mut ActorHandlerContext) -> Result<()> {
        let lock_by = self
            .channel_lock_by_resource()
            .expect("expect lock_by resource exists");

        let time_lock = lock_by.time_lock;

        let watch_tag = {
            let mut t = self.channel_address.to_vec();
            t.extend("lock".as_bytes());
            t
        };

        let mut timeout_receiver = self
            .chain_txn_watcher
            .add_interest(watch_tag, Box::new(move |txn| txn.block_id > time_lock))
            .await?;
        let mut self_ref = self.actor_ref(ctx).await;

        tokio::task::spawn(async move {
            while let Some(_) = timeout_receiver.next().await {
                match self_ref.send(ChannelLockTimeout).await {
                    Err(_) => break,
                    Ok(Ok(_)) => break,
                    Ok(Err(e)) => {
                        error!(
                            "actor fail to handle channel lock timeout, continue watch, err: {}",
                            e
                        );
                    }
                }
            }
        });

        Ok(())
    }

    /// clear local pending state
    fn clear_pending_txn(&self) -> Result<()> {
        self.store.clear_pending_txn()
    }

    fn channel_op_to_action(
        &self,
        op: &ChannelOp,
        args: Vec<TransactionArgument>,
    ) -> Result<ScriptAction> {
        match op {
            ChannelOp::Open => {
                let module_id =
                    ModuleId::new(AccountAddress::default(), Identifier::new("ChannelScript")?);

                Ok(ScriptAction::new_call(
                    module_id,
                    Identifier::new("open")?,
                    args,
                ))
            }
            ChannelOp::Close => {
                let module_id =
                    ModuleId::new(AccountAddress::default(), Identifier::new("LibraAccount")?);

                Ok(ScriptAction::new_call(
                    module_id,
                    Identifier::new("close")?,
                    args,
                ))
            }
            ChannelOp::Execute {
                package_name,
                script_name,
            } => {
                let script_code = self
                    .script_registry
                    .get_script(package_name, script_name)
                    .ok_or(format_err!(
                        "Can not find script by package {} and script name {}",
                        package_name,
                        script_name
                    ))?;
                Ok(ScriptAction::new_code(
                    script_code.byte_code().clone(),
                    args,
                ))
            }
            ChannelOp::Action {
                module_address,
                module_name,
                function_name,
            } => {
                let module_id = ModuleId::new(
                    module_address.clone(),
                    Identifier::new(module_name.clone().into_boxed_str())?,
                );
                Ok(ScriptAction::new_call(
                    module_id,
                    Identifier::new(function_name.clone().into_boxed_str())?,
                    args,
                ))
            }
        }
    }
}

pub fn channel_txn_interest(channel_address: AccountAddress) -> Interest {
    Box::new(move |txn_info| match &txn_info.txn {
        Transaction::UserTransaction(t) => match t.raw_txn().payload() {
            TransactionPayload::Channel(cp) => cp.channel_address() == channel_address,
            _ => false,
        },
        _ => false,
    })
}

#[allow(dead_code)]
pub fn channel_txn_oneshot_interest(
    channel_address: AccountAddress,
    channel_sequence_number: u64,
) -> Interest {
    Box::new(move |txn| match &txn.txn {
        Transaction::UserTransaction(t) => match t.raw_txn().payload() {
            TransactionPayload::Channel(cp) => {
                cp.channel_address() == channel_address
                    && cp.channel_sequence_number() == channel_sequence_number
            }
            _ => false,
        },
        _ => false,
    })
}
