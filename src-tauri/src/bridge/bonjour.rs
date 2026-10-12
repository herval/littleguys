//! `_littleguys._tcp` on the LAN, so a paired phone finds this Mac after DHCP
//! moves it. TXT carries `v=1` and `k=<macId>`; no names.

use mdns_sd::{ServiceDaemon, ServiceInfo};

const SERVICE: &str = "_littleguys._tcp.local.";

pub struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
}

pub fn advertise(mac_id: &str, port: u16) -> anyhow::Result<Advert> {
    let daemon = ServiceDaemon::new()?;
    let instance = format!("littleguys-{}", &mac_id[..6.min(mac_id.len())]);
    let host = format!("{instance}.local.");
    let props = [("v", "1"), ("k", mac_id)];
    let info = ServiceInfo::new(SERVICE, &instance, &host, "", port, &props[..])?.enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;
    Ok(Advert { daemon, fullname })
}

impl Drop for Advert {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}
