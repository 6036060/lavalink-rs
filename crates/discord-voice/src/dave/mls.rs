//! openmls による MLS グループ管理（フェーズ A, feature = "dave-mls"）。
//!
//! ciphersuite = MLS_128_DHKEMP256_AES128GCM_SHA256_P256（0x0002, 署名 P256）。
//! base secret = export_secret("Discord Secure Frames v0", LE(sender_id), 16)。
//!
//! ⚠️ Discord との実際の MLS 交換（external sender 組込み・proposal→commit・welcome の
//! 正確な TLS 形式）は実機検証が必須。本実装は openmls API に沿った best-effort で、
//! 末尾の `local_two_member_exporter_matches` で openmls 機構＋exporter を検証する。
//! 実機ログ（opcode 生バイト）を見ながら順次精緻化する前提。

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use super::session::MlsBackend;

pub const DAVE_CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
pub const FRAMES_LABEL: &str = "Discord Secure Frames v0";

fn make_identity(user_id: u64) -> (SignatureKeyPair, CredentialWithKey) {
    let signer = SignatureKeyPair::new(DAVE_CIPHERSUITE.signature_algorithm())
        .expect("generate P256 signature keypair");
    let credential = BasicCredential::new(user_id.to_be_bytes().to_vec());
    let cwk = CredentialWithKey {
        credential: credential.into(),
        signature_key: signer.public().into(),
    };
    (signer, cwk)
}

fn build_key_package(
    provider: &OpenMlsRustCrypto,
    signer: &SignatureKeyPair,
    cwk: CredentialWithKey,
) -> KeyPackage {
    // 公式 op26 に合わせ capabilities を DAVE プロファイル最小限にする
    // （version mls10 / ciphersuite 2 のみ / extensions・proposals は空 / basic credential）。
    let caps = Capabilities::new(
        Some(&[ProtocolVersion::Mls10]),
        Some(&[DAVE_CIPHERSUITE]),
        Some(&[] as &[ExtensionType]),
        Some(&[] as &[ProposalType]),
        Some(&[CredentialType::Basic]),
    );
    KeyPackage::builder()
        .leaf_node_capabilities(caps)
        .build(DAVE_CIPHERSUITE, provider, signer, cwk)
        .expect("build key package")
        .key_package()
        .clone()
}

pub struct OpenMlsBackend {
    provider: OpenMlsRustCrypto,
    signer: SignatureKeyPair,
    credential: CredentialWithKey,
    group: Option<MlsGroup>,
    external_sender: Option<ExternalSender>,
    /// Discord 側グループの group_id（プロポーザルから採用）。
    pending_group_id: Option<GroupId>,
    /// 直近に自分が送った commit(MLSMessage バイト)。op29 で自コミットを識別するため。
    last_commit: Option<Vec<u8>>,
    user_id: u64,
}

impl OpenMlsBackend {
    pub fn new(user_id: u64) -> Self {
        let provider = OpenMlsRustCrypto::default();
        let (signer, credential) = make_identity(user_id);
        Self {
            provider,
            signer,
            credential,
            group: None,
            external_sender: None,
            pending_group_id: None,
            last_commit: None,
            user_id,
        }
    }
    pub fn user_id(&self) -> u64 {
        self.user_id
    }
}

impl MlsBackend for OpenMlsBackend {
    fn set_external_sender(&mut self, payload: &[u8]) {
        match ExternalSender::tls_deserialize_exact(payload) {
            Ok(es) => self.external_sender = Some(es),
            Err(e) => tracing::warn!(error = ?e, "parse external sender failed"),
        }
    }

    fn create_group(&mut self) -> Option<Vec<u8>> {
        // op26 用: MLSMessage ラップした key package。
        let kp = build_key_package(&self.provider, &self.signer, self.credential.clone());
        let kp_bytes = MlsMessageOut::from(kp).tls_serialize_detached().ok()?;

        // external sender extension を含めてローカルグループを作成。
        let extensions = match &self.external_sender {
            Some(es) => Extensions::single(Extension::ExternalSenders(vec![es.clone()])),
            None => Extensions::empty(),
        };
        // leaf capabilities に Add/Remove を明示（openmls の ValSem113 対策）。
        let capabilities = Capabilities::new(
            None,
            None,
            None,
            Some(&[ProposalType::Add, ProposalType::Remove]),
            None,
        );
        let mut builder = MlsGroup::builder()
            .ciphersuite(DAVE_CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            // 仕様: handshake は平文 PublicMessage で送る（gateway が検証できるよう）。
            .with_wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .with_capabilities(capabilities);
        if let Some(gid) = self.pending_group_id.clone() {
            // Discord 側グループの group_id を採用（ランダムだと WrongGroupId で拒否される）。
            builder = builder.with_group_id(gid);
        }
        let built = match builder.with_group_context_extensions(extensions) {
            Ok(b) => b.build(&self.provider, &self.signer, self.credential.clone()),
            Err(e) => {
                tracing::warn!(error = ?e, "mls group extension failed");
                return Some(kp_bytes);
            }
        };
        match built {
            Ok(group) => {
                tracing::info!(
                    group_id = ?group.group_id().as_slice(),
                    epoch = group.epoch().as_u64(),
                    has_external_sender = self.external_sender.is_some(),
                    leaf_caps = ?group.own_leaf_node().map(|l| l.capabilities().clone()),
                    "dave: local group created"
                );
                self.group = Some(group);
                // 再作成時は古いコミット履歴を捨てる。
                self.last_commit = None;
            }
            Err(e) => tracing::warn!(error = ?e, "mls create_group failed"),
        }
        Some(kp_bytes)
    }

    fn handle_proposals(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        // payload = [operation_type(u8)][append: MLSMessage proposals<V>]。append のみ対応。
        if payload.first().copied() != Some(0) {
            return None;
        }
        let rest = &payload[1..];
        let (vec_len, hdr) = read_mls_varint(rest)?;
        let content = rest.get(hdr..hdr + vec_len as usize)?;
        tracing::info!(op_type = payload[0], vec_len, content_len = content.len(), "dave: handling proposals");
        tracing::info!(
            proposals_hex = %content.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            "dave: raw proposals FULL HEX (for ref diff)"
        );

        // 先頭プロポーザルの group_id/epoch を覗き、未作成なら同じ group_id でグループを作る。
        if self.group.is_none() {
            let mut peek: &[u8] = content;
            if let Ok(m) = MlsMessageIn::tls_deserialize(&mut peek) {
                if let Ok(pm) = m.try_into_protocol_message() {
                    tracing::info!(
                        proposal_group_id = ?pm.group_id().as_slice(),
                        proposal_epoch = pm.epoch().as_u64(),
                        "dave: incoming proposal group context"
                    );
                    self.pending_group_id = Some(pm.group_id().clone());
                }
            }
            self.create_group();
        }

        // 仕様: コミットは external sender のプロポーザルを「参照(by reference)」しなければならない。
        // 各 external Add/Remove を process_message → store_pending_proposal で pending に積む。
        let group = self.group.as_mut()?;
        let mut n_staged = 0u32;
        let mut cur = content;
        while !cur.is_empty() {
            let mut slice: &[u8] = cur;
            let msg = match MlsMessageIn::tls_deserialize(&mut slice) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = ?e, "dave: proposal deserialize stopped");
                    break;
                }
            };
            let consumed = cur.len() - slice.len();
            if consumed == 0 {
                break;
            }
            cur = &cur[consumed..];
            match msg.try_into_protocol_message() {
                Ok(protocol) => match group.process_message(&self.provider, protocol) {
                    Ok(processed) => match processed.into_content() {
                        ProcessedMessageContent::ProposalMessage(prop) => {
                            if group.store_pending_proposal(self.provider.storage(), *prop).is_ok() {
                                n_staged += 1;
                            }
                        }
                        _ => tracing::warn!("dave: processed message was not a proposal"),
                    },
                    Err(e) => tracing::warn!(error = ?e, "dave: process_message(proposal) REJECTED"),
                },
                Err(e) => tracing::warn!(error = ?e, "dave: try_into_protocol_message failed"),
            }
        }
        tracing::info!(n_staged, "dave: proposals staged by reference");
        if n_staged == 0 {
            tracing::warn!("dave: no proposals staged; not committing");
            return None;
        }

        // 既存の保留コミットがあれば破棄してから作り直す（新しい op27/revoke が来た場合の
        // GroupStateError(PendingCommit) を回避）。
        let _ = group.clear_pending_commit(self.provider.storage());
        // pending proposals を参照によりコミット（Add を含むと welcome 付き）。
        let (commit, welcome_opt, _gi) = group
            .commit_to_pending_proposals(&self.provider, &self.signer)
            .map_err(|e| tracing::warn!(error = ?e, "commit_to_pending_proposals failed"))
            .ok()?;
        let has_welcome = welcome_opt.is_some();
        let commit_bytes = commit.tls_serialize_detached().ok()?;
        // 診断: commit の先頭 = [version u16][wire_format u16]。public=0001, private=0002。
        tracing::info!(commit_head = ?&commit_bytes[..commit_bytes.len().min(6)], "dave: commit head");
        let mut out = commit_bytes.clone();
        if let Some(welcome) = welcome_opt {
            // op28 は raw Welcome を要求する。openmls は MLSMessage ラップで返すので
            // 先頭の MLSMessage ヘッダ(version u16 + wire_format u16 = 4 バイト)を剥がす。
            let welcome_msg = welcome.tls_serialize_detached().ok()?;
            // 診断: MLSMessage ラップなら先頭 [0001 0003], raw Welcome なら [0002...]。
            tracing::info!(welcome_head = ?&welcome_msg[..welcome_msg.len().min(6)], "dave: welcome head (pre-strip)");
            if welcome_msg.len() > 4 {
                out.extend_from_slice(&welcome_msg[4..]);
            }
        }
        // 自分のコミットを即マージして epoch を進める。
        // 即 merge しない。gateway が op29 で確定コミットを announce するまで保留する。
        // 自分のコミットが勝てば op29 で merge、別メンバーのが勝てばそれを適用する（apply_commit）。
        // 即 merge すると、自分のコミットが負けた場合にグループ状態が全体と乖離し無音になる。
        tracing::info!(
            epoch = group.epoch().as_u64(),
            commit_welcome_len = out.len(),
            has_welcome,
            "dave: committed by reference (pending announce)"
        );
        self.last_commit = Some(commit_bytes);
        Some(out)
    }

    fn apply_commit(&mut self, commit: &[u8]) -> bool {
        let is_own = self.last_commit.as_deref() == Some(commit);
        let Some(group) = self.group.as_mut() else {
            return false;
        };
        if is_own {
            // 自分のコミットが採用された → 保留中のコミットを merge して epoch を進める。
            match group.merge_pending_commit(&self.provider) {
                Ok(()) => {
                    tracing::info!(epoch = group.epoch().as_u64(), "dave: merged our own announced commit");
                    true
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "merge_pending_commit failed");
                    false
                }
            }
        } else {
            // 別メンバーのコミットが採用された → 自分の保留コミットを破棄して相手のを適用。
            let _ = group.clear_pending_commit(self.provider.storage());
            let Ok(msg) = MlsMessageIn::tls_deserialize_exact(commit) else {
                return false;
            };
            let Ok(protocol) = msg.try_into_protocol_message() else {
                return false;
            };
            match group.process_message(&self.provider, protocol) {
                Ok(processed) => match processed.into_content() {
                    ProcessedMessageContent::StagedCommitMessage(staged) => {
                        let ok = group.merge_staged_commit(&self.provider, *staged).is_ok();
                        if ok {
                            tracing::info!(epoch = group.epoch().as_u64(), "dave: applied other member's announced commit");
                        }
                        ok
                    }
                    _ => false,
                },
                Err(e) => {
                    tracing::warn!(error = ?e, "mls apply_commit failed");
                    false
                }
            }
        }
    }

    fn join_welcome(&mut self, welcome: &[u8]) -> bool {
        // 仕様: op30 は raw Welcome（MLSMessage ラップではない）。
        let Ok(welcome) = Welcome::tls_deserialize_exact(welcome) else {
            tracing::warn!("dave: welcome deserialize failed");
            return false;
        };
        let cfg = MlsGroupJoinConfig::default();
        match StagedWelcome::new_from_welcome(&self.provider, &cfg, welcome, None) {
            Ok(staged) => match staged.into_group(&self.provider) {
                Ok(group) => {
                    self.group = Some(group);
                    true
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "mls into_group failed");
                    false
                }
            },
            Err(e) => {
                tracing::warn!(error = ?e, "mls staged welcome failed");
                false
            }
        }
    }

    fn sender_base_secret(&self, sender_id: u64) -> Option<[u8; 16]> {
        let group = self.group.as_ref()?;
        let secret = group
            .export_secret(&self.provider, FRAMES_LABEL, &sender_id.to_le_bytes(), 16)
            .ok()?;
        let mut out = [0u8; 16];
        out.copy_from_slice(secret.as_slice());
        Some(out)
    }

    fn epoch(&self) -> u64 {
        self.group.as_ref().map(|g| g.epoch().as_u64()).unwrap_or(0)
    }

    fn key_package(&mut self) -> Option<Vec<u8>> {
        // 秘密鍵は self.provider に保存される（後で listener のコミットが勝った場合の
        // welcome 参加で使われる）。
        // 実機キャプチャ: op26 は raw KeyPackage（MLSMessage ラップではない）。openmls は
        // MLSMessage ラップ [version u16][wire_format=5][KeyPackage] で返すので先頭4バイトを剥がす。
        let kp = build_key_package(&self.provider, &self.signer, self.credential.clone());
        let wrapped = MlsMessageOut::from(kp).tls_serialize_detached().ok()?;
        if wrapped.len() > 4 {
            Some(wrapped[4..].to_vec())
        } else {
            None
        }
    }
}

/// MLS 可変長ベクタの長さプレフィックス(RFC 9420 §2.1.2)を読む。(値, ヘッダ長)。
fn read_mls_varint(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    match first >> 6 {
        0 => Some(((first & 0x3F) as u64, 1)),
        1 => Some(((((first & 0x3F) as u64) << 8) | (*b.get(1)? as u64), 2)),
        2 => {
            let mut v = (first & 0x3F) as u64;
            for i in 1..4 {
                v = (v << 8) | (*b.get(i)? as u64);
            }
            Some((v, 4))
        }
        _ => {
            let mut v = (first & 0x3F) as u64;
            for i in 1..8 {
                v = (v << 8) | (*b.get(i)? as u64);
            }
            Some((v, 8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_two_member_exporter_matches() {
        let alice_provider = OpenMlsRustCrypto::default();
        let bob_provider = OpenMlsRustCrypto::default();
        let (alice_signer, alice_cwk) = make_identity(1);
        let (bob_signer, bob_cwk) = make_identity(2);

        let bob_kp = build_key_package(&bob_provider, &bob_signer, bob_cwk);

        let mut alice_group = MlsGroup::builder()
            .ciphersuite(DAVE_CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .build(&alice_provider, &alice_signer, alice_cwk)
            .expect("create alice group");

        let (_commit, welcome, _gi) = alice_group
            .add_members(&alice_provider, &alice_signer, &[bob_kp.into()])
            .expect("add bob");
        alice_group.merge_pending_commit(&alice_provider).expect("merge");

        let welcome_bytes = welcome.tls_serialize_detached().expect("ser welcome");
        let welcome_in = MlsMessageIn::tls_deserialize_exact(&welcome_bytes).expect("de welcome");
        let welcome = match welcome_in.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => panic!("expected welcome"),
        };
        let bob_group = StagedWelcome::new_from_welcome(
            &bob_provider,
            &MlsGroupJoinConfig::default(),
            welcome,
            None,
        )
        .expect("staged")
        .into_group(&bob_provider)
        .expect("join");

        let a = alice_group
            .export_secret(&alice_provider, FRAMES_LABEL, &2u64.to_le_bytes(), 16)
            .unwrap();
        let b = bob_group
            .export_secret(&bob_provider, FRAMES_LABEL, &2u64.to_le_bytes(), 16)
            .unwrap();
        assert_eq!(a, b);
    }
}
