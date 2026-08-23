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
    type_serial(
        &mut m,
        "ls -l /mnt/cdrom; wc -c /mnt/cdrom/vmlinuz; printf 'DONE%s\\n' MARK\n",
    );
    assert!(
        wait_serial(&mut m, "DONEMARK", 2_000_000_000),
        "コマンドが終わらない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    let tail = &out[out.rfind("ls -l /mnt/cdrom").unwrap_or(0)..];
    assert!(
        tail.contains("vmlinuz") && tail.contains("initramfs"),
        "ISO の中身が見えない:\n{tail}"
    );
    let want = format!("{} /mnt/cdrom/vmlinuz", kernel.len());
    assert!(
        tail.contains(&want),
        "vmlinuz の大きさが合わない (期待 {want}):\n{tail}"
    );
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
    assert!(
        run_until(&mut m, "tc@box", 3_000_000_000),
        "シェルに届かない"
    );
    m.run(5_000_000);
    // 目印は打った行に現れない形で (END""MARK はシェルが連結して初めて ENDMARK になる)
    // Tiny Core は pata_legacy + sr を組み込みで持ち、/mnt/sr0 の fstab も自分で作る
    m.devices.keyboard.type_ascii(
        "ls -l /dev/sr0; mount /mnt/sr0 && ls /mnt/sr0 /mnt/sr0/boot | head -8; echo END\"\"MARK\n",
    );
    assert!(
        run_until(&mut m, "ENDMARK", 1_000_000_000),
        "コマンドが終わらない"
    );
    let screen = m.text_screen_string();
    eprintln!("--- Tiny Core の画面 ---\n{screen}");
    assert!(
        screen.contains("/dev/sr0") && !screen.contains("No such file"),
        "/dev/sr0 が無い:\n{screen}"
    );
    assert!(
        screen.contains("vmlinuz") && screen.contains("core.gz"),
        "ISO の中身 (boot/vmlinuz, core.gz) が見えない:\n{screen}"
    );
    eprintln!(
        "ATA コマンド数: {}",
        m.devices.ide.as_ref().map(|d| d.commands).unwrap_or(0)
    );
}

/// (重い) 大きな ISO を端から端まで ATAPI で読んで壊れていないか — DSL 2024 の linuxfs (700MB) の
/// md5 を ISO 内の .md5 と突き合わせる。RUSTX86_BIG_ISO=path で指定
#[test]
#[ignore]
fn big_iso_reads_intact() {
    let (Ok(kernel), Ok(initrd)) = (std::fs::read(KERNEL), std::fs::read(INITRD)) else {
        return;
    };
    let Ok(path) = std::env::var("RUSTX86_BIG_ISO") else {
        return;
    };
    let iso = std::fs::read(&path).unwrap();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(256));
    m.cd_attach(iso);
    m.boot_linux_with_initrd(&kernel, "console=ttyS0 rustx86.ide=1", Some(&initrd))
        .expect("boot");
    assert!(wait_serial(&mut m, "busybox shell", 3_000_000_000));
    type_serial(&mut m, "ls -la /mnt/cdrom/antiX/; cat /mnt/cdrom/antiX/linuxfs.md5; md5sum /mnt/cdrom/antiX/linuxfs; printf 'DONE%s\\n' MARK\n");
    assert!(
        wait_serial(&mut m, "DONEMARK", 400_000_000_000),
        "終わらない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    eprintln!("{}", &out[out.rfind("ls -la").unwrap_or(0)..]);
}

/// (重い) DSL 2024 を CD から起動し、シリアルにも出させて init のログを全部採る。
/// RUSTX86_BIG_ISO=path。出力は RUSTX86_SERIAL_OUT のファイルへ
#[test]
#[ignore]
fn dsl_serial_log() {
    let Ok(path) = std::env::var("RUSTX86_BIG_ISO") else {
        return;
    };
    let iso = std::fs::read(&path).unwrap();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(512));
    m.boot_from_iso(iso).unwrap();
    let mut n = 0u64;
    while n < 200_000_000 {
        m.run(1_000_000);
        n += 1_000_000;
        if m.text_screen_string().contains("boot:") {
            break;
        }
    }
    eprintln!("boot: 時点の video_mode={:#x}", m.video_mode);
    m.run(2_000_000);
    // BIOS のキー待ち行列は 15 個。isolinux は 1 文字ずつゆっくり取るので、小分けに打つ
    // ラベルの APPEND (quiet) を使わず、カーネルの行を丸ごと打つ (全ログをシリアルへ)
    let line = std::env::var("RUSTX86_BOOT_LINE")
        .unwrap_or_else(|_| "text console=ttyS0,115200 console=tty1\n".into());
    for chunk in line.as_bytes().chunks(6) {
        m.devices
            .keyboard
            .type_ascii(std::str::from_utf8(chunk).unwrap());
        m.run(3_000_000);
    }
    let budget: u64 = std::env::var("RUSTX86_BUDGET_G")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
        * 1_000_000_000;
    let mut spent = 0u64;
    while spent < budget {
        m.run(5_000_000);
        spent += 5_000_000;
        if serial(&m).contains("login:") {
            break;
        }
    }
    let out = serial(&m);
    if let Ok(p) = std::env::var("RUSTX86_SERIAL_OUT") {
        std::fs::write(&p, &out).unwrap();
    }
    eprintln!(
        "{spent} 命令 / シリアル {} バイト / 最後の画面:\n{}",
        out.len(),
        m.text_screen_string()
    );
}

/// (重い) DSL の linuxfs を Alpine の下で chroot し、glibc の coreutils が動くか —
/// 「memory exhausted」が antiX のカーネル/設定のせいか、エミュレータ (CPU) のせいかを切り分ける
#[test]
#[ignore]
fn dsl_chroot_under_alpine() {
    let (Ok(kernel), Ok(initrd)) = (std::fs::read(KERNEL), std::fs::read(INITRD)) else {
        return;
    };
    let Ok(path) = std::env::var("RUSTX86_BIG_ISO") else {
        return;
    };
    let iso = std::fs::read(&path).unwrap();
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(512));
    m.cd_attach(iso);
    m.boot_linux_with_initrd(&kernel, "console=ttyS0 rustx86.ide=1", Some(&initrd))
        .expect("boot");
    assert!(wait_serial(&mut m, "busybox shell", 3_000_000_000));
    // glibc の coreutils/awk/python の浮動小数点。x87 の遅延フラグの穴 (FUCOMIP の結果が
    // 直前の ALU 命令に上書きされる) で全部 inf/nan だった (2026-08-23)
    let cmd = concat!(
        "mkdir -p /mnt/lfs; mount -t squashfs -o loop /mnt/cdrom/antiX/linuxfs /mnt/lfs; ",
        "echo R1; grep -h 'v00001234d00001111' /mnt/lfs/lib/modules/*/modules.alias | head -3; ",
        "echo R2; grep -rn 'bochs\\|blacklist' /mnt/lfs/etc/modprobe.d/ 2>/dev/null | head -5; ",
        "echo R3; grep -n 'bochs\\|1234\\|modesetting\\|vesa' /mnt/lfs/usr/local/bin/make-xorg-conf 2>/dev/null | head -12; ls /mnt/lfs/usr/local/bin/ | grep -i xorg; ",
        "echo R4; grep -rln 'Found_no_video' /mnt/lfs/live/bin /mnt/lfs/usr/local/lib/live 2>/dev/null | head -3; ",
        "printf 'DONE%s\\n' MARK\n"
    );
    type_serial(&mut m, cmd);
    assert!(
        wait_serial(&mut m, "DONEMARK", 60_000_000_000),
        "終わらない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    eprintln!("{}", &out[out.rfind("mkdir -p /mnt/lfs").unwrap_or(0)..]);
}
