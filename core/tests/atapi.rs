//! ATAPI の CD-ROM (6c 2段目)。IDE secondary (0x170/0x376、IRQ15) の素子を、
//! Linux の pata_legacy + sr_mod + isofs が /dev/sr0 として掴み、ISO の中身が読めること。
use rustx86_core::{Machine, MachineProfile};

const KERNEL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/vmlinuz-lts");
const INITRD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/initramfs-mini");
const ISO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/test-linux.iso");
const TC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/Core-current.iso");

fn serial(m: &Machine) -> String {
    String::from_utf8_lossy(&m.devices.uart.tx).into_owned()
}

fn type_serial(m: &mut Machine, s: &str) {
    for b in s.bytes() {
        m.devices.uart.rx.push_back(b);
    }
}

/// シリアルに needle が出るまで回す (上限 budget 命令)
fn wait_serial(m: &mut Machine, needle: &str, budget: u64) -> bool {
    let mut spent = 0u64;
    while spent < budget {
        m.run(2_000_000);
        spent += 2_000_000;
        if serial(m).contains(needle) {
            return true;
        }
    }
    false
}

/// Linux (カーネル直ロード) に CD を挿し、init が rustx86.ide で ATAPI を掴んで
/// /mnt/cdrom に掛ける。ISO の中の vmlinuz が読めれば合格
#[test]
fn linux_mounts_iso_via_atapi() {
    let (Ok(kernel), Ok(initrd), Ok(iso)) = (
        std::fs::read(KERNEL),
        std::fs::read(INITRD),
        std::fs::read(ISO),
    ) else {
        eprintln!("skip: images/vmlinuz-lts・initramfs-mini・test-linux.iso のどれかが無い");
        return;
    };
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.cd_attach(iso.clone());
    m.boot_linux_with_initrd(&kernel, "console=ttyS0 rustx86.ide=1", Some(&initrd))
        .expect("boot");
    assert!(
        wait_serial(&mut m, "busybox shell", 3_000_000_000),
        "シェルに届かない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    assert!(
        out.contains("cdrom: /dev/sr0 を /mnt/cdrom に掛けた"),
        "init が CD を掛けていない。dmesg:\n{out}"
    );
    // 中身を読む: ISO の /vmlinuz は images/vmlinuz-lts そのもの
    type_serial(&mut m, "ls -l /mnt/cdrom; wc -c /mnt/cdrom/vmlinuz; printf 'DONE%s\\n' MARK\n");
    assert!(wait_serial(&mut m, "DONEMARK", 2_000_000_000), "コマンドが終わらない:\n{}", serial(&m));
    let out = serial(&m);
    let tail = &out[out.rfind("ls -l /mnt/cdrom").unwrap_or(0)..];
    assert!(tail.contains("vmlinuz") && tail.contains("initramfs"), "ISO の中身が見えない:\n{tail}");
    let want = format!("{} /mnt/cdrom/vmlinuz", kernel.len());
    assert!(tail.contains(&want), "vmlinuz の大きさが合わない (期待 {want}):\n{tail}");
    let ide = m.devices.ide.as_ref().unwrap();
    eprintln!("ATA コマンド数: {}", ide.commands);
}

/// Tiny Core の実物 ISO: 起動後に /dev/sr0 が居て、ISO 自身が読めるか
/// (Tiny Core のカーネルが ATAPI を持っているかの実測。無ければ台帳)
#[test]
fn tinycore_sees_its_own_cd() {
    let Ok(iso) = std::fs::read(TC) else {
        eprintln!("skip: {TC} が無い");
        return;
    };
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_from_iso(iso).expect("El Torito");
    let run_until = |m: &mut Machine, needle: &str, budget: u64| -> bool {
        let mut i = 0u64;
        while i < budget {
            m.run(1_000_000);
            i += 1_000_000;
            if m.text_screen_string().contains(needle) {
                return true;
            }
        }
        false
    };
    assert!(run_until(&mut m, "boot:", 100_000_000), "boot: が出ない");
    for _ in 0..2 {
        m.run(2_000_000);
        m.devices.keyboard.type_ascii("\n");
    }
    assert!(run_until(&mut m, "tc@box", 3_000_000_000), "シェルに届かない");
    m.run(5_000_000);
    m.devices.keyboard.type_ascii("clear; ls -l /dev/sr0; dmesg | grep -iE 'ata2|sr0|pata|scsi' | tail -5; echo ENDMARK\n");
    assert!(run_until(&mut m, "ENDMARK", 1_000_000_000), "コマンドが終わらない");
    let screen = m.text_screen_string();
    eprintln!("--- Tiny Core の画面 ---\n{screen}");
    eprintln!("ATA コマンド数: {}", m.devices.ide.as_ref().map(|d| d.commands).unwrap_or(0));
}
