//! ブートセクタの実行テスト。asm/以下のnasm成果物 (コミット済み) を使う。

use rustx86_core::Machine;

#[test]
fn boot_sector_hello_world() {
    let sector = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../asm/hello.bin"))
        .expect("asm/hello.bin (nasm -f bin hello.asm でビルド)");
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    let executed = m.run(10_000);
    assert!(m.halted, "HLT到達せず ({executed}命令実行)");
    assert_eq!(m.console_string(), "Hello, World!");
}

#[test]
fn rejects_sector_without_signature() {
    let mut m = Machine::new();
    assert!(m.load_boot_sector(&[0u8; 512]).is_err());
}
