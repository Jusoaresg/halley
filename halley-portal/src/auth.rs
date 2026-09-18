//! Authenticate the portal frontend, then pin its unique bus identity on objects.
use zbus::names::{BusName, OwnedUniqueName};
use zbus::{blocking::Connection, fdo, message::Header};

pub fn frontend(connection: &Connection, header: &Header<'_>) -> fdo::Result<OwnedUniqueName> {
    let sender = header.sender().ok_or_else(denied)?;
    let bus = zbus::blocking::fdo::DBusProxy::new(connection)?;
    let owner = bus
        .get_name_owner(BusName::try_from("org.freedesktop.portal.Desktop").unwrap())
        .map_err(|_| denied())?;
    same_owner(sender.as_str(), owner.as_str())?;
    let name = BusName::Unique(sender.clone());
    let uid = bus
        .get_connection_unix_user(name.clone())
        .map_err(|_| denied())?;
    let pid = bus
        .get_connection_unix_process_id(name)
        .map_err(|_| denied())?;
    if uid != unsafe { libc::geteuid() } {
        return Err(denied());
    }
    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|_| denied())?;
    let approved = std::env::var_os("HALLEY_PORTAL_FRONTEND").map(std::path::PathBuf::from);
    let paths = approved.map(|path| vec![path]).unwrap_or_else(|| {
        vec![
            "/usr/lib/xdg-desktop-portal".into(),
            "/usr/libexec/xdg-desktop-portal".into(),
        ]
    });
    if !paths
        .iter()
        .filter(|path| path.is_absolute())
        .any(|path| std::fs::canonicalize(path).is_ok_and(|path| path == executable))
    {
        return Err(denied());
    }
    Ok(owner)
}

pub fn same_owner(sender: &str, owner: &str) -> fdo::Result<()> {
    if sender.starts_with(':') && sender == owner {
        Ok(())
    } else {
        Err(denied())
    }
}

fn denied() -> fdo::Error {
    fdo::Error::AccessDenied(
        "only the authenticated xdg-desktop-portal frontend may access this object".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_replaced_and_well_known_owners() {
        assert!(same_owner(":1.2", ":1.2").is_ok());
        assert!(same_owner(":1.3", ":1.2").is_err());
        assert!(same_owner("", "").is_err());
        assert!(
            same_owner(
                "org.freedesktop.portal.Desktop",
                "org.freedesktop.portal.Desktop"
            )
            .is_err()
        );
    }
}
