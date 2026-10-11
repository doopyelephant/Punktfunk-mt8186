//! `punktfunk-seats fence`: udev's `IMPORT{program}` for a seat's pad
//! (`packaging/linux/65-punktfunk-seats.rules`). It reads the device's `phys` out of sysfs and
//! prints the seat user it names, so the rule can hand the node to that user alone. A device it
//! cannot map prints nothing, and the node keeps the distro's rules. A device under a vhci port
//! is the binary's to map, by the port the supervisor recorded.

use super::accounts;
use std::path::Path;

/// What the supervisor stamps on a seat's pad: `punktfunk-seat:<account>/<index>`.
const PHYS_PREFIX: &str = "punktfunk-seat:";

/// The udev program. Never fails: udev takes the lines it gets.
pub fn run() {
    let Some(devpath) = std::env::var_os("DEVPATH") else {
        return;
    };
    let dev = Path::new("/sys").join(
        Path::new(&devpath)
            .strip_prefix("/")
            .unwrap_or(Path::new(&devpath)),
    );
    if let Some(account) = seat_of_sysfs(&dev) {
        print_for(&account);
    }
}

/// The rule's lines for `account`, when it names a user other than root.
pub fn print_for(account: &str) {
    let Ok(Some(user)) = accounts::lookup(account) else {
        return;
    };
    if user.uid != 0 {
        print!("{}", lines(account, user.uid));
    }
}

/// What the rule reads: the user, its uid and the seat name the node goes on.
pub fn lines(account: &str, uid: u32) -> String {
    format!("PF_SEAT_USER={account}\nPF_SEAT_UID={uid}\nPF_SEAT=seat-punktfunk-{account}\n")
}

/// The account stamped on `dev` or its nearest stamped ancestor: an input device's `phys`, or a
/// HID device's `HID_PHYS` in its `uevent`. A uhid pad's driver can leave its own input devices'
/// `phys` empty, so the stamp sits on the HID device up to three levels above the node.
fn seat_of_sysfs(dev: &Path) -> Option<String> {
    dev.ancestors().take(5).find_map(|d| {
        let phys = std::fs::read_to_string(d.join("phys")).ok();
        let hid = std::fs::read_to_string(d.join("uevent"))
            .ok()
            .and_then(|uevent| hid_phys(&uevent));
        [phys, hid]
            .into_iter()
            .flatten()
            .find_map(|stamp| account_of_phys(stamp.trim()))
    })
}

/// `HID_PHYS=` out of a HID device's `uevent`.
fn hid_phys(uevent: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_PHYS="))
        .map(str::to_owned)
}

/// `punktfunk-seat:<account>/<index>` → the account, when it is a plain user name.
pub fn account_of_phys(phys: &str) -> Option<String> {
    let rest = phys.strip_prefix(PHYS_PREFIX)?;
    let account = rest.split('/').next()?;
    let plain = !account.is_empty()
        && account.len() <= 32
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain.then(|| account.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stamped_plain_account_is_fenced() {
        assert_eq!(
            account_of_phys("punktfunk-seat:pf-seat-1/0"),
            Some("pf-seat-1".into())
        );
        assert_eq!(
            account_of_phys("punktfunk-seat:bazzite/3"),
            Some("bazzite".into())
        );
        assert_eq!(account_of_phys("usb-0000:00:14.0-1/input0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:/0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:../root/0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:a b/0"), None);
    }

    #[test]
    fn a_hid_devices_uevent_names_its_phys() {
        let uevent = "DRIVER=playstation\nHID_ID=0003:0000054C:00000CE6\n\
                      HID_NAME=Punktfunk DualSense 0\nHID_PHYS=punktfunk-seat:pf-seat-1/dualsense/0\n\
                      HID_UNIQ=punktfunk-ds-0\nMODALIAS=hid:b0003g0001v0000054Cp00000CE6\n";
        assert_eq!(
            hid_phys(uevent).as_deref().and_then(account_of_phys),
            Some("pf-seat-1".into())
        );
        assert_eq!(hid_phys("DRIVER=x\n"), None);
    }

    /// hid-playstation's input devices leave `phys` empty: the node's stamp is the HID device's,
    /// three levels up. A node with no stamp anywhere above is nobody's.
    #[test]
    fn a_uhid_pads_input_node_finds_the_hid_devices_stamp() {
        let root = std::env::temp_dir().join(format!("pf-fence-{}", std::process::id()));
        let hid = root.join("uhid/0003:054C:0CE6.0014");
        let input = hid.join("input/input77");
        std::fs::create_dir_all(input.join("event5")).unwrap();
        std::fs::write(input.join("phys"), "\n").unwrap();
        std::fs::write(
            hid.join("uevent"),
            "HID_PHYS=punktfunk-seat:pf-seat-1/dualsense/0\n",
        )
        .unwrap();
        assert_eq!(
            seat_of_sysfs(&input.join("event5")),
            Some("pf-seat-1".into())
        );
        std::fs::write(hid.join("uevent"), "HID_PHYS=usb-0000:00:14.0-1/input0\n").unwrap();
        assert_eq!(seat_of_sysfs(&input.join("event5")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_rule_reads_three_lines() {
        assert_eq!(
            lines("pf-seat-1", 987),
            "PF_SEAT_USER=pf-seat-1\nPF_SEAT_UID=987\nPF_SEAT=seat-punktfunk-pf-seat-1\n"
        );
    }
}
