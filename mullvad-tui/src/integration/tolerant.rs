// SPDX-License-Identifier: GPL-3.0-or-later

//! Version-tolerant gRPC client wrapper.
//!
//! Wraps upstream's [`MullvadProxyClient`] and replaces the small set of
//! read-side methods that flow through strict `TryFrom<proto::*>` conversions
//! in `mullvad-management-interface/src/types/conversions/`. Those upstream
//! conversions treat unset `Option<sub-message>` fields and unknown enum/
//! oneof discriminants as fatal (via `.ok_or(...)?`), so a daemon out of
//! sync with our consumed proto - in either direction - causes the
//! conversion (and the wrapping RPC) to fail at the very first call.
//!
//! Two skew directions to defend against, both handled by the same
//! patcher framework:
//!
//! - **Older daemon** (lacks a sub-message field our consumed proto strict- requires): pre-fill the
//!   missing `Option<sub-message>` with a default or sentinel before handing to upstream's
//!   `TryFrom`. The patcher tables here are currently empty for this direction because at the
//!   `2026.2` pin every field our consumed proto requires has been around long enough that no
//!   daemon we'd realistically meet would omit it.
//!
//! - **Newer daemon** (sends an enum/oneof discriminant our consumed proto doesn't know about):
//!   prost decodes the unknown variant as `None` on the inner oneof, which then fails upstream's
//!   `TryFrom`. The fix is the same shape: replace the would-be-rejected sub-message with a
//!   sentinel before upstream sees it, so the rest of the surrounding `Settings` decode succeeds.
//!   Currently this protects the API access methods (each method's `AccessMethod` oneof gained
//!   `DomainFronting` in `mullvad-app/origin/main` after `2026.2`).
//!
//! Per-stream resilience: [`TolerantClient::events_listen`] drops events
//! that fail to decode rather than ending the stream, so one unknown
//! `DaemonEvent` variant from a newer daemon doesn't kill the listener.
//! `get_settings` stays strict - the patcher should be sufficient, and
//! failing loudly there is the only signal we'd get for a new skew
//! direction we haven't covered yet.
//!
//! Mutator methods flow through [`Deref`]/[`DerefMut`] to the underlying
//! [`MullvadProxyClient`] unchanged. The TUI never round-trips a `Settings`
//! struct (it writes via individual `set_*` calls), so the write direction
//! has no skew hazard. Calls to RPCs the daemon doesn't recognize surface
//! as `UNIMPLEMENTED` through the existing operation-status notification
//! path - no special handling here.
//!
//! When bumping the `mullvadvpn-app` submodule pin, run
//! `git -C mullvadvpn-app diff <old>..<new> -- mullvad-management-interface/src/types/conversions/`
//! and add a `patch_*` line for every new `.ok_or(...)?` on an
//! `Option<sub-message>` field, plus a fixture leg in the byte-level test
//! at the bottom of this file.
//!
//! **Dual-pin compatibility.** The source tree compiles against both the
//! latest stable `mullvadvpn-app` tag and a tip-of-`main` bump so the
//! user can build releases against the stable daemon and develop against
//! upstream-in-flight features in parallel. To keep both pins building,
//! every patcher entry that references a *newly-added* proto field is
//! `#[cfg(daemon_has_<feature>)]`-gated. The flag is set by the build
//! script (`mullvad-tui/build.rs`) when it finds the field's declaration
//! in the active proto file. When adding a new patcher entry for a
//! future bump:
//!
//! 1. Add a `(flag, marker)` row to the `FEATURES` table in `build.rs`.
//! 2. Gate the patcher's `coerce_*` call / struct-literal field / fixture leg with
//!    `#[cfg(daemon_has_<flag>)]`.
//! 3. The fixture leg should set the new field to `None` under the cfg so the regression test
//!    exercises the missing-on-the-wire case the patcher exists to handle.

use std::ops::{Deref, DerefMut};

use futures::{Stream, StreamExt};
use mullvad_management_interface::{
    Error, ManagementServiceClient, MullvadProxyClient, client::DaemonEvent, types as proto,
};
use mullvad_types::settings::Settings;

type Result<T> = std::result::Result<T, Error>;

/// Sentinel UUID for synthesized access-method entries when an older daemon
/// doesn't populate a slot upstream's `TryFrom` requires. Surfaces to the
/// user (when api-access UI is exposed) as a clearly-disabled row, not a
/// real method, so the synthesized default doesn't mask the gap.
const SENTINEL_UUID: &str = "00000000-0000-0000-0000-000000000000";
const SENTINEL_NAME: &str = "<unsupported by daemon>";

/// Wraps a [`MullvadProxyClient`] alongside a raw [`ManagementServiceClient`]
/// that share the underlying `tonic::transport::Channel`. Mutator calls flow
/// through `Deref`/`DerefMut` to the proxy unchanged; read calls that go
/// through strict `TryFrom`s are shadowed by inherent methods on this type.
#[derive(Debug, Clone)]
pub struct TolerantClient {
    proxy: MullvadProxyClient,
    raw: ManagementServiceClient,
}

impl TolerantClient {
    pub async fn new() -> Result<Self> {
        #[expect(deprecated)]
        let raw = mullvad_management_interface::new_management_service_client().await?;
        let proxy = MullvadProxyClient::from_rpc_client(raw.clone());
        Ok(Self { proxy, raw })
    }

    /// Shadow [`MullvadProxyClient::get_settings`] with a version-tolerant
    /// version. Receives the raw [`proto::Settings`], pre-fills any
    /// `Option<sub-message>` field upstream's `Settings::try_from` requires,
    /// then hands off to upstream.
    pub async fn get_settings(&mut self) -> Result<Settings> {
        let mut settings = self.raw.get_settings(()).await?.into_inner();
        patch_settings(&mut settings);
        Settings::try_from(settings).map_err(Error::InvalidResponse)
    }

    /// Shadow [`MullvadProxyClient::events_listen`]. Patches each item's
    /// `Settings` variant before handing off to upstream's
    /// `DaemonEvent::try_from`. Other event variants pass through unchanged
    /// because their `TryFrom`s have no new strict-required fields under
    /// the current submodule pin; a future bump that broke another variant
    /// would extend [`patch_daemon_event`].
    ///
    /// Per-item resilience: if a single event fails to decode (unknown
    /// oneof variant from a newer daemon, missing required field from an
    /// older one), it's logged and dropped rather than ending the stream.
    /// Transport-level failures from the underlying gRPC stream do still
    /// propagate - those signal a connection issue, not a wire-shape skew.
    pub async fn events_listen<'a>(
        &mut self,
    ) -> Result<impl Stream<Item = Result<DaemonEvent>> + 'a> {
        let listener = self.raw.events_listen(()).await?.into_inner();
        Ok(listener.filter_map(|item| async move {
            decode_event_item(item.map_err(|status| Error::Rpc(Box::new(status))))
        }))
    }
}

impl Deref for TolerantClient {
    type Target = MullvadProxyClient;
    fn deref(&self) -> &Self::Target {
        &self.proxy
    }
}

impl DerefMut for TolerantClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.proxy
    }
}

// ---------------------------------------------------------------------------
// Patcher table
// ---------------------------------------------------------------------------

/// Patch a [`proto::Settings`] in place so upstream's `Settings::try_from`
/// (in `mullvad-management-interface/src/types/conversions/settings.rs` and
/// neighbors) accepts it across both skew directions documented at the top
/// of this module.
///
/// Scope is intentionally narrow: only fields known to differ across recent
/// submodule pins. Long-required fields (e.g. `relay_settings`, the
/// `wireguard_constraints` oneof inner) are *not* defaulted here because
/// daemons have always populated them and synthesizing a default for a
/// nested oneof is complex and unlikely to be useful. If a future bump
/// adds another skew-vulnerable field, append a line below and a leg in
/// the byte-level fixture test.
///
/// Idempotent: a fully-populated, fully-known `Settings` is unchanged.
fn patch_settings(s: &mut proto::Settings) {
    // Older daemons (predating LWO landing in `mullvadvpn-app/main`)
    // don't populate `obfuscation_settings.lwo`. Upstream's
    // `TryFrom<ObfuscationSettings>` strict-requires it, so synthesize
    // a default `Lwo` slot before handing off. Only compiled when the
    // submodule pin actually has the `Lwo` proto message (see `build.rs`).
    #[cfg(daemon_has_lwo)]
    if let Some(obf) = s.obfuscation_settings.as_mut()
        && obf.lwo.is_none()
    {
        tracing::warn!(
            field = "ObfuscationSettings.lwo",
            "patching missing daemon field with synthesized default; version skew suspected"
        );
        obf.lwo = Some(proto::obfuscation_settings::Lwo::default());
    }
    if s.api_access_methods.is_none() {
        tracing::warn!(
            field = "Settings.api_access_methods",
            "patching missing daemon field with synthesized defaults; version skew suspected"
        );
        s.api_access_methods = Some(synthesize_api_access_methods());
    } else if let Some(api) = s.api_access_methods.as_mut() {
        coerce_access_method_slot(&mut api.direct, "ApiAccessMethodSettings.direct");
        coerce_access_method_slot(
            &mut api.mullvad_bridges,
            "ApiAccessMethodSettings.mullvad_bridges",
        );
        coerce_access_method_slot(
            &mut api.encrypted_dns_proxy,
            "ApiAccessMethodSettings.encrypted_dns_proxy",
        );
        // The `domain_fronting` built-in only exists on submodule pins
        // that include the proto field; cfg-gated so the same source
        // tree compiles against an older pin too.
        #[cfg(daemon_has_domain_fronting)]
        coerce_access_method_slot(
            &mut api.domain_fronting,
            "ApiAccessMethodSettings.domain_fronting",
        );
        coerce_custom_access_methods(&mut api.custom);
    }
}

fn patch_daemon_event(event: &mut proto::daemon_event::Event) {
    if let proto::daemon_event::Event::Settings(s) = event {
        patch_settings(s);
    }
}

/// Run the per-event read pipeline (unwrap envelope, patch, upstream
/// `TryFrom`) and convert the three "bad item, not bad stream" failure
/// modes into `None` so [`futures::StreamExt::filter_map`] drops them:
///
/// - transport-level error in the stream item itself -> propagate (the connection is in trouble;
///   `event_listener_loop` will reconnect),
/// - empty event envelope (`MissingDaemonEvent`) -> log + drop,
/// - upstream `DaemonEvent::try_from` rejected the patched event (unknown oneof variant or missing
///   required sub-field) -> log + drop.
fn decode_event_item(item: Result<proto::DaemonEvent>) -> Option<Result<DaemonEvent>> {
    match item {
        Err(error) => Some(Err(error)),
        Ok(envelope) => match envelope.event {
            None => {
                tracing::warn!(
                    "daemon emitted an empty event envelope; dropping and keeping listener alive"
                );
                None
            }
            Some(mut event) => {
                patch_daemon_event(&mut event);
                match DaemonEvent::try_from(event) {
                    Ok(decoded) => Some(Ok(decoded)),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "daemon event failed to decode (likely version skew); dropping and keeping listener alive"
                        );
                        None
                    }
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Patcher helpers
// ---------------------------------------------------------------------------

fn synthesize_api_access_methods() -> proto::ApiAccessMethodSettings {
    proto::ApiAccessMethodSettings {
        direct: Some(sentinel_access_method_setting()),
        mullvad_bridges: Some(sentinel_access_method_setting()),
        encrypted_dns_proxy: Some(sentinel_access_method_setting()),
        #[cfg(daemon_has_domain_fronting)]
        domain_fronting: Some(sentinel_access_method_setting()),
        custom: Vec::new(),
    }
}

/// Replace a built-in `AccessMethodSetting` slot with the sentinel when it
/// won't survive upstream's strict `TryFrom`. Two cases:
///
/// - **Slot missing entirely** (`None`): the daemon predates this built-in and didn't populate the
///   field on the wire. Older daemons run into this with every new built-in upstream lands (most
///   recently `domain_fronting`).
/// - **Slot present but inner oneof unknown** (`Some` with `None` inner): the daemon emitted an
///   `AccessMethod` variant our consumed proto doesn't recognize. Prost surfaces unknown oneof
///   variants as `None` on the inner field.
///
/// Without this, upstream's `TryFrom<AccessMethodSetting>` would
/// `.ok_or(...)?` and fail the whole `Settings` decode. Idempotent: a
/// slot whose oneof is already a known variant is unchanged.
fn coerce_access_method_slot(slot: &mut Option<proto::AccessMethodSetting>, name: &'static str) {
    if slot.is_none() {
        tracing::warn!(
            field = name,
            "daemon access-method slot is missing (likely older daemon predating this built-in); \
             substituting sentinel"
        );
        *slot = Some(sentinel_access_method_setting());
        return;
    }
    if access_method_oneof_unknown(slot.as_ref()) {
        tracing::warn!(
            field = name,
            "daemon access-method has unknown oneof variant; substituting sentinel"
        );
        *slot = Some(sentinel_access_method_setting());
    }
}

/// Walk the `custom` access-method list and replace any entry whose
/// `AccessMethod` oneof prost couldn't decode with the sentinel. Replaces
/// in place rather than dropping so the row count the user sees on the
/// API access page is preserved (the sentinel renders as
/// `<unsupported by daemon>`).
fn coerce_custom_access_methods(custom: &mut [proto::AccessMethodSetting]) {
    for (i, entry) in custom.iter_mut().enumerate() {
        if access_method_oneof_unknown(Some(&*entry)) {
            tracing::warn!(
                index = i,
                field = "ApiAccessMethodSettings.custom",
                "custom access-method has unknown oneof variant; substituting sentinel"
            );
            *entry = sentinel_access_method_setting();
        }
    }
}

/// True if the `AccessMethodSetting` is present but its inner
/// `AccessMethod` oneof didn't decode (either the wrapping `AccessMethod`
/// message is `None` or the `AccessMethod.access_method` oneof itself is
/// `None`). A fully-`None` outer slot returns `false` here - the
/// missing-slot case is handled by [`coerce_access_method_slot`]'s
/// upfront `None` check, not by this predicate.
fn access_method_oneof_unknown(slot: Option<&proto::AccessMethodSetting>) -> bool {
    let Some(setting) = slot else {
        return false;
    };
    match setting.access_method.as_ref() {
        None => true,
        Some(am) => am.access_method.is_none(),
    }
}

fn sentinel_access_method_setting() -> proto::AccessMethodSetting {
    proto::AccessMethodSetting {
        id: Some(proto::Uuid {
            value: SENTINEL_UUID.to_owned(),
        }),
        name: SENTINEL_NAME.to_owned(),
        enabled: false,
        access_method: Some(proto::AccessMethod {
            access_method: Some(proto::access_method::AccessMethod::Direct(
                proto::access_method::Direct {},
            )),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `proto::Settings` populated as a `2026.2` daemon would emit.
    /// Long-required fields (relay_settings, dns_options, etc.) are filled
    /// the same way a real daemon fills them. At the current submodule pin
    /// the patcher table is empty, so this fixture exists primarily to
    /// guard against future bumps: the regression test below composes
    /// fixture -> patch_settings -> upstream `Settings::try_from`, and a
    /// new `.ok_or(...)?` field upstream picks up will fail it.
    fn settings_2026_2_shape() -> proto::Settings {
        proto::Settings {
            relay_settings: Some(proto::RelaySettings {
                endpoint: Some(proto::relay_settings::Endpoint::Normal(
                    proto::NormalRelaySettings {
                        location: None,
                        providers: Vec::new(),
                        wireguard_constraints: Some(proto::WireguardConstraints::default()),
                        ownership: 0,
                    },
                )),
            }),
            allow_lan: false,
            lockdown_mode: false,
            auto_connect: false,
            tunnel_options: Some(proto::TunnelOptions {
                dns_options: Some(proto::DnsOptions {
                    state: 0,
                    default_options: Some(proto::DefaultDnsOptions::default()),
                    custom_options: Some(proto::CustomDnsOptions::default()),
                }),
                quantum_resistant: Some(proto::QuantumResistantState::default()),
                daita: Some(proto::DaitaSettings::default()),
                ..Default::default()
            }),
            show_beta_releases: false,
            split_tunnel: Some(proto::SplitTunnelSettings::default()),
            // 2026.2 daemons predate the LWO obfuscation, so the `lwo`
            // slot is missing on the wire (prost decodes it as `None`).
            // The patcher fills it before upstream's TryFrom runs. Cfg-
            // gated so the same fixture compiles against the 2026.2 pin
            // (where the `lwo` field doesn't exist on the proto struct).
            obfuscation_settings: Some(proto::ObfuscationSettings {
                selected_obfuscation: 0,
                udp2tcp: Some(proto::obfuscation_settings::Udp2TcpObfuscation::default()),
                shadowsocks: Some(proto::obfuscation_settings::Shadowsocks::default()),
                wireguard_port: Some(proto::obfuscation_settings::WireguardPort::default()),
                #[cfg(daemon_has_lwo)]
                lwo: None,
            }),
            custom_lists: Some(proto::CustomListSettings::default()),
            // 2026.2 daemons predate the `domain_fronting` built-in so the
            // slot is missing on the wire. The patcher fills it with a
            // sentinel before upstream's TryFrom runs. Cfg-gated for the
            // same reason as `lwo` above.
            api_access_methods: Some(proto::ApiAccessMethodSettings {
                direct: Some(non_sentinel_access_method("direct")),
                mullvad_bridges: Some(non_sentinel_access_method("mullvad_bridges")),
                encrypted_dns_proxy: Some(non_sentinel_access_method("encrypted_dns_proxy")),
                #[cfg(daemon_has_domain_fronting)]
                domain_fronting: None,
                custom: Vec::new(),
            }),
            relay_overrides: Vec::new(),
            recents: None,
            update_default_location: false,
        }
    }

    fn non_sentinel_access_method(name: &str) -> proto::AccessMethodSetting {
        proto::AccessMethodSetting {
            id: Some(proto::Uuid {
                value: "11111111-2222-3333-4444-555555555555".to_owned(),
            }),
            name: name.to_owned(),
            enabled: true,
            access_method: Some(proto::AccessMethod {
                access_method: Some(proto::access_method::AccessMethod::Direct(
                    proto::access_method::Direct {},
                )),
            }),
        }
    }

    #[test]
    fn patch_settings_synthesizes_missing_api_access_methods() {
        // Removing `api_access_methods` entirely is the failure mode that
        // could plausibly happen if a daemon predates that field.
        let mut s = settings_2026_2_shape();
        s.api_access_methods = None;
        patch_settings(&mut s);
        let api = s.api_access_methods.unwrap();
        assert_eq!(api.direct.unwrap().name, SENTINEL_NAME);
        assert_eq!(api.mullvad_bridges.unwrap().name, SENTINEL_NAME);
        assert_eq!(api.encrypted_dns_proxy.unwrap().name, SENTINEL_NAME);
    }

    #[test]
    fn patch_settings_preserves_existing_access_methods() {
        let mut s = settings_2026_2_shape();
        patch_settings(&mut s);
        let api = s.api_access_methods.unwrap();
        assert_eq!(api.direct.unwrap().name, "direct");
        assert_eq!(api.mullvad_bridges.unwrap().name, "mullvad_bridges");
        assert_eq!(api.encrypted_dns_proxy.unwrap().name, "encrypted_dns_proxy");
    }

    #[test]
    fn patch_settings_is_idempotent_on_already_patched() {
        let mut s = settings_2026_2_shape();
        patch_settings(&mut s);
        let after_first = format!("{s:?}");
        patch_settings(&mut s);
        let after_second = format!("{s:?}");
        assert_eq!(after_first, after_second);
    }

    /// **The regression-catching test.** Build a 2026.2-shape `proto::Settings`,
    /// run the patcher, then upstream's `Settings::try_from`. Asserts the
    /// composition succeeds. If a future submodule bump introduces a new
    /// strict-required `Option<sub-msg>` field that the patcher table
    /// doesn't know about, this test fails - and the failure points
    /// directly at the missing patcher entry.
    #[test]
    fn patch_then_upstream_try_from_accepts_2026_2_shape() {
        let mut s = settings_2026_2_shape();
        patch_settings(&mut s);
        let result = Settings::try_from(s);
        assert!(
            result.is_ok(),
            "patched 2026.2-shape Settings rejected by upstream TryFrom: {:?}",
            result.err()
        );
    }

    #[test]
    fn patch_daemon_event_settings_path_is_patched() {
        let mut s = settings_2026_2_shape();
        s.api_access_methods = None;
        let mut event = proto::daemon_event::Event::Settings(s);
        patch_daemon_event(&mut event);
        let proto::daemon_event::Event::Settings(s) = event else {
            unreachable!("Settings variant constructed above")
        };
        assert!(s.api_access_methods.is_some());
    }

    /// An `AccessMethodSetting` whose inner `AccessMethod.access_method`
    /// oneof is `None` is exactly what prost decodes when a newer daemon
    /// emits a variant our consumed proto doesn't know about (e.g.
    /// `DomainFronting` from `mullvadvpn-app/origin/main`).
    fn access_method_with_unknown_oneof(name: &str) -> proto::AccessMethodSetting {
        proto::AccessMethodSetting {
            id: Some(proto::Uuid {
                value: "deadbeef-dead-beef-dead-beefdeadbeef".to_owned(),
            }),
            name: name.to_owned(),
            enabled: true,
            access_method: Some(proto::AccessMethod {
                access_method: None,
            }),
        }
    }

    #[test]
    fn coerce_access_method_substitutes_sentinel_for_unknown_oneof_variant() {
        let mut slot = Some(access_method_with_unknown_oneof("future-method"));
        coerce_access_method_slot(&mut slot, "test.slot");
        let patched = slot.expect("slot remains populated");
        assert_eq!(patched.name, SENTINEL_NAME);
        assert_eq!(patched.id.unwrap().value, SENTINEL_UUID);
        assert!(!patched.enabled);
    }

    #[test]
    fn coerce_access_method_leaves_known_variant_untouched() {
        let mut slot = Some(non_sentinel_access_method("known-method"));
        coerce_access_method_slot(&mut slot, "test.slot");
        assert_eq!(slot.unwrap().name, "known-method");
    }

    #[test]
    fn coerce_access_method_fills_missing_slot_with_sentinel() {
        // An older daemon that predates a built-in won't populate the
        // slot at all. The coercer fills it so upstream's strict
        // `.ok_or(...)?` accepts the slot.
        let mut slot: Option<proto::AccessMethodSetting> = None;
        coerce_access_method_slot(&mut slot, "test.slot");
        let patched = slot.expect("missing slot must be filled");
        assert_eq!(patched.name, SENTINEL_NAME);
        assert_eq!(patched.id.unwrap().value, SENTINEL_UUID);
        assert!(!patched.enabled);
    }

    #[test]
    fn patch_custom_methods_replaces_unknown_variants_in_place() {
        let mut s = settings_2026_2_shape();
        let api = s.api_access_methods.as_mut().unwrap();
        api.custom = vec![
            non_sentinel_access_method("custom-1"),
            access_method_with_unknown_oneof("custom-future"),
            non_sentinel_access_method("custom-3"),
        ];
        patch_settings(&mut s);
        let custom = &s.api_access_methods.unwrap().custom;
        assert_eq!(custom.len(), 3, "row count must be preserved");
        assert_eq!(custom[0].name, "custom-1");
        assert_eq!(custom[1].name, SENTINEL_NAME);
        assert_eq!(custom[2].name, "custom-3");
    }

    /// Forward-compat regression test: build a `proto::Settings` whose
    /// `custom` access-method list contains an entry with the unknown-oneof
    /// shape a newer daemon would produce, run the patcher, then upstream's
    /// `Settings::try_from`. Asserts the composition succeeds. Symmetric
    /// counterpart of [`patch_then_upstream_try_from_accepts_2026_2_shape`]:
    /// guards future bumps that move types in either direction.
    #[test]
    fn patch_then_upstream_try_from_accepts_unknown_custom_method() {
        let mut s = settings_2026_2_shape();
        s.api_access_methods
            .as_mut()
            .unwrap()
            .custom
            .push(access_method_with_unknown_oneof("custom-future"));
        patch_settings(&mut s);
        let result = Settings::try_from(s);
        assert!(
            result.is_ok(),
            "patched Settings with unknown-variant custom method rejected by upstream TryFrom: {:?}",
            result.err()
        );
    }

    /// `patch_settings` must catch unknown-variant in built-in slots too.
    /// If a daemon ever emits e.g. an `encrypted_dns_proxy` slot whose
    /// `AccessMethod` oneof is unknown to us, the slot becomes the sentinel
    /// rather than dragging the whole `Settings` decode down.
    #[test]
    fn patch_settings_replaces_unknown_builtin_variant_with_sentinel() {
        let mut s = settings_2026_2_shape();
        s.api_access_methods.as_mut().unwrap().encrypted_dns_proxy =
            Some(access_method_with_unknown_oneof("future-edns"));
        patch_settings(&mut s);
        let edns = s
            .api_access_methods
            .unwrap()
            .encrypted_dns_proxy
            .expect("slot remains populated");
        assert_eq!(edns.name, SENTINEL_NAME);
    }

    #[test]
    fn decode_event_item_drops_empty_envelope() {
        let envelope = proto::DaemonEvent { event: None };
        let result = decode_event_item(Ok(envelope));
        assert!(result.is_none(), "empty envelope must be dropped");
    }

    #[test]
    fn decode_event_item_propagates_transport_errors() {
        // Transport-level error must surface to the consumer (not be
        // dropped) so reconnect/backoff logic above can run.
        let result = decode_event_item(Err(Error::MissingDaemonEvent));
        match result {
            Some(Err(_)) => {}
            other => panic!("expected Some(Err(_)), got {other:?}"),
        }
    }

    #[test]
    fn decode_event_item_passes_known_settings_event_through() {
        let s = settings_2026_2_shape();
        let envelope = proto::DaemonEvent {
            event: Some(proto::daemon_event::Event::Settings(s)),
        };
        let decoded = decode_event_item(Ok(envelope))
            .expect("known event yields Some")
            .expect("known event decodes Ok");
        match decoded {
            DaemonEvent::Settings(_) => {}
            other => panic!("expected Settings, got {other:?}"),
        }
    }
}
