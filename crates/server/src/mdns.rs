//! mDNS advertisement glue (TDD §3.2/§9) on top of the pure TXT contract in
//! `blt_core::discovery`. Advertises `_blt._tcp` with **IP:port TXT records**
//! (never `.local` hostnames, #9) for the game + share services; the admin
//! panel is deliberately not advertised.

use crate::bindings::advertise_ip;
use crate::state::SharedState;
use blt_core::discovery::{build_txt, AdvertisedServer, SERVICE_TYPE};
use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use tracing::{info, warn};

/// Register (or re-register) the advertisement. Returns the daemon so the
/// caller keeps it alive; dropping it stops advertising.
pub fn advertise(state: &SharedState) -> Option<ServiceDaemon> {
    let cfg = state.config.read();
    let game_bind = match cfg.game_bind() {
        Ok(b) => b,
        Err(e) => {
            warn!("mdns: bad game bind: {e}");
            return None;
        }
    };
    let share_bind = cfg.share_bind().ok();

    let game_ip = advertise_ip(&game_bind);
    let adv = AdvertisedServer {
        uuid: cfg.server_uuid.clone(),
        label: cfg.server_label.clone(),
        game_endpoint: format!("{game_ip}:{}", game_bind.port()),
        share_endpoint: share_bind
            .map(|b| format!("{}:{}", advertise_ip(&b), b.port()))
            .unwrap_or_default(),
    };
    drop(cfg);

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mdns daemon failed to start (discovery disabled, manual IP still works): {e}");
            return None;
        }
    };
    // Discovery is IPv4-only (the TXT contract carries IPv4 IP:port). Skip IPv6
    // so the responder doesn't choke on link-local addrs (en0 fe80::, awdl0) and
    // spew "Cannot find valid addrs ... V6(fe80::…)" every ~90s.
    let _ = daemon.disable_interface(IfKind::IPv6);

    let txt = build_txt(&adv);
    let instance = format!("BLT-{}", &adv.uuid[..8.min(adv.uuid.len())]);
    let host = format!("{game_ip}");
    let info = match ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &format!("{instance}.local."),
        host,
        game_bind.port(),
        txt.into_iter().collect::<std::collections::HashMap<_, _>>(),
    ) {
        Ok(i) => i,
        Err(e) => {
            warn!("mdns service info: {e}");
            return None;
        }
    };
    match daemon.register(info) {
        Ok(()) => {
            info!(label = %adv.label, game = %adv.game_endpoint, share = %adv.share_endpoint,
                  "mDNS advertising as {SERVICE_TYPE}");
            Some(daemon)
        }
        Err(e) => {
            warn!("mdns register: {e}");
            None
        }
    }
}
