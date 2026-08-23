//! Bochs VGA (PCI 1234:1111、DISPI) を Linux の bochs (tiny DRM) が掴み、fbdev エミュレーションの
//! /dev/fb0 に書いた画素が LFB (RAM 末尾の VRAM) に現れること。6f の土台
use rustx86_core::{Machine, MachineProfile};

const KERNEL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/vmlinuz-lts");
const INITRD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/initramfs-mini");

fn serial(m: &Machine) -> String {
    String::from_utf8_lossy(&m.devices.uart.tx).into_owned()
}

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

#[test]
fn bochs_drm_exposes_fb0_and_pixels_land_in_vram() {
    let (Ok(kernel), Ok(initrd)) = (std::fs::read(KERNEL), std::fs::read(INITRD)) else {
        eprintln!("skip: images/vmlinuz-lts・initramfs-mini が無い");
        return;
    };
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.vga_attach();
    assert!(m.vram_base.is_some(), "PCI 機なら VRAM が切り出される");
    m.boot_linux_with_initrd(&kernel, "console=ttyS0 rustx86.vga=1", Some(&initrd))
        .expect("boot");
    assert!(
        wait_serial(&mut m, "busybox shell", 3_000_000_000),
        "シェルに届かない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    assert!(
        out.contains("vga: bochs-drm が /dev/fb0 を生やした"),
        "bochs が掴んでいない。ログ:\n{out}"
    );
    // bochs が掴んだ時点で fbcon がモードを組む → lfb が出来ている
    let lfb = m.lfb.expect("DISPI の ENABLE で lfb が組まれる");
    assert!(m.lfb_xrgb, "Bochs の画素は XRGB");
    eprintln!("lfb: {}x{} @ {:#x}", lfb.width, lfb.height, lfb.base);
    // 赤 (XRGB = 00 00 FF 00 の並び: B,G,R,X) を fb0 の先頭に 16KB 書く
    let cmd = "dmesg | grep -iE 'BAR 0|00:05.0' | head -4; cat /sys/bus/pci/devices/0000:00:05.0/resource | head -1; cat /sys/class/graphics/fb0/name /sys/class/graphics/fb0/virtual_size /sys/class/graphics/fb0/bits_per_pixel; \
printf '\\000\\000\\377\\000' > /tmp/px; for i in 1 2 3 4 5 6 7 8 9 10 11 12; do cat /tmp/px /tmp/px > /tmp/px2; mv /tmp/px2 /tmp/px; done; \
dd if=/tmp/px of=/dev/fb0 bs=16384 count=1 2>&1 | tail -1; printf 'DONE%s\\n' MARK\n";
    for b in cmd.bytes() {
        m.devices.uart.rx.push_back(b);
    }
    assert!(
        wait_serial(&mut m, "DONEMARK", 2_000_000_000),
        "終わらない:\n{}",
        serial(&m)
    );
    let out = serial(&m);
    let tail = &out[out.rfind("cat /sys/class").unwrap_or(0)..];
    eprintln!("{tail}");
    assert!(
        tail.contains("bochs-drm") || tail.contains("bochsdrmfb"),
        "fb0 の名前が bochs でない:\n{tail}"
    );
    // bochs (tiny) の fbdev は**影のバッファ** (shmem) に書き、deferred I/O の worker が
    // 少し遅れて VRAM へ写す (damage)。写る時間を与える
    m.run(200_000_000);
    let fb = m.lfb_frame();
    assert!(fb.len() >= 16384);
    // 先頭の画素が赤 (B=0,G=0,R=FF)。fbcon が描いた文字で上書きされていなければ
    let red = fb[..16384]
        .chunks_exact(4)
        .filter(|p| p[0] == 0 && p[1] == 0 && p[2] == 0xFF)
        .count();
    assert!(
        red > 2000,
        "赤い画素が LFB に現れない (red={red}): 先頭 16 バイト {:02x?}",
        &fb[..16]
    );
}
