//! デバッガの検証。
//!
//! **道具の検証も要る。** このリポジトリでは、検証用に書いたスクリプト自身に
//! バグがあって46件の偽の不一致を出したことがある。デバッガが嘘をつくと
//! 本体のバグより厄介なので、ここは厚めに書く。

use rustx86_core::debug::Stop;
use rustx86_core::{cpu, Machine};

/// 0x7C00 に置いて動かすための下ごしらえ
fn boot(code: &[u8]) -> Machine {
    let mut m = Machine::new();
    let mut sector = vec![0u8; 512];
    sector[..code.len()].copy_from_slice(code);
    sector[510] = 0x55;
    sector[511] = 0xAA;
    m.load_boot_sector(&sector).unwrap();
    m
}

#[test]
fn 切っている間は命令を数えない() {
    // 元締めが切れていれば、フックはどれも通らない。
    // **速度を守る仕掛けが効いていること**をここで押さえる
    let mut m = boot(&[0x90, 0x90, 0x90]); // NOP×3
    for _ in 0..3 {
        m.step();
    }
    assert!(!m.dbg.on);
    assert_eq!(m.dbg.instr, 0);
}

#[test]
fn ブレークポイントは命令を実行する前に止まる() {
    // MOV AX,0x1234 / NOP
    let mut m = boot(&[0xB8, 0x34, 0x12, 0x90]);
    m.dbg.break_at(0x7C00);
    m.step();
    assert_eq!(m.dbg.stop, Some(Stop::Break(0x7C00)));
    // **まだ実行していない**。止まった状態でその命令を見られるのが要点
    assert_eq!(m.cpu.regs[cpu::AX], 0);
    assert_eq!(m.cpu.ip, 0x7C00);
}

#[test]
fn 止まった場所から先へ進める() {
    // これが無いと同じブレークポイントで永久に止まり続ける
    let mut m = boot(&[0xB8, 0x34, 0x12, 0x90]);
    m.dbg.break_at(0x7C00);
    m.step();
    assert!(m.dbg.take_stop().is_some());
    m.step(); // 一度だけ見逃して実行される
    assert_eq!(m.cpu.regs[cpu::AX], 0x1234);
    assert!(m.dbg.stop.is_none());
}

#[test]
fn 番地を書いた命令の位置まで分かる() {
    // MOV AX,0x00AA / MOV [0x0450],AL
    let code = [0xB8, 0xAA, 0x00, 0xA2, 0x50, 0x04];
    let mut m = boot(&code);
    m.write8(0x0450, 0x0E); // 元の値
    m.dbg.watch_mem(0x0450);
    for _ in 0..2 {
        m.step();
    }
    match m.dbg.take_stop() {
        Some(Stop::WriteMem { addr, old, new, at }) => {
            assert_eq!((addr, old, new), (0x0450, 0x0E, 0xAA));
            // **書いた命令の先頭**であって、書き込み時点のIPではない
            assert_eq!(at, (0x0000, 0x7C03));
        }
        other => panic!("書き込みを捕まえていない: {other:?}"),
    }
}

#[test]
fn 監視していない番地では止まらない() {
    let code = [0xB8, 0xAA, 0x00, 0xA2, 0x50, 0x04];
    let mut m = boot(&code);
    m.dbg.watch_mem(0x0451); // 隣を見張る
    for _ in 0..2 {
        m.step();
    }
    assert_eq!(m.dbg.stop, None);
    assert_eq!(m.read8(0x0450), 0xAA);
}

#[test]
fn ポートに書いた値と場所が分かる() {
    // MOV DX,0x3D4 / MOV AL,0x0C / OUT DX,AL
    //
    // `OUT imm8,AL` (0xE6) では**ポート番号が8bitしか入らない**ので
    // 0x3D4 は指定できない。CRTCのように0x100より上の装置はDX形式になる
    let mut m = boot(&[0xBA, 0xD4, 0x03, 0xB0, 0x0C, 0xEE]);
    m.dbg.watch_io(0x3D4, false, true);
    for _ in 0..3 {
        m.step();
    }
    match m.dbg.take_stop() {
        Some(Stop::WriteIo { port, val, at }) => {
            assert_eq!((port, val), (0x3D4, 0x0C));
            assert_eq!(at, (0x0000, 0x7C05));
        }
        other => panic!("I/O書き込みを捕まえていない: {other:?}"),
    }
}

#[test]
fn ポートが返した値まで分かる() {
    // **装置が何を答えたか**が分からないと、OSがなぜその判断をしたのか追えない。
    // 0x0080 は誰も繋がっていないので 0xFF が返る (ISAのプルアップ)
    let mut m = boot(&[0xE4, 0x80]); // IN AL,0x80
    m.dbg.watch_io(0x80, true, false);
    m.step();
    match m.dbg.take_stop() {
        Some(Stop::ReadIo { port, val, .. }) => assert_eq!((port, val), (0x80, 0xFF)),
        other => panic!("I/O読み出しを捕まえていない: {other:?}"),
    }
}

#[test]
fn ワード幅のアクセスもフックを通る() {
    // 16bit/32bitのアクセスは8bit版を2回呼ぶ形で書かれている。
    // **ここが独立に実装されていると監視をすり抜ける**ので押さえておく
    // MOV DX,0x3D4 / MOV AX,0x0C0E / OUT DX,AX
    let mut m = boot(&[0xBA, 0xD4, 0x03, 0xB8, 0x0E, 0x0C, 0xEF]);
    m.dbg.watch_io(0x3D5, false, true); // **上位バイトが行く先**を見張る
    for _ in 0..3 {
        m.step();
    }
    match m.dbg.take_stop() {
        Some(Stop::WriteIo { port, val, .. }) => assert_eq!((port, val), (0x3D5, 0x0C)),
        other => panic!("ワード幅のI/Oを捕まえていない: {other:?}"),
    }

    // メモリ側も同じ
    let mut m = boot(&[0xB8, 0x34, 0x12, 0xA3, 0x50, 0x04]); // MOV AX,0x1234 / MOV [0x450],AX
    m.dbg.watch_mem(0x0451);
    for _ in 0..2 {
        m.step();
    }
    match m.dbg.take_stop() {
        Some(Stop::WriteMem { addr, new, .. }) => assert_eq!((addr, new), (0x0451, 0x12)),
        other => panic!("ワード幅のメモリ書き込みを捕まえていない: {other:?}"),
    }
}

#[test]
fn 指定した命令数の手前で止まる() {
    let mut m = boot(&[0x90; 16]);
    m.dbg.run_to(5);
    for _ in 0..20 {
        m.step();
        if m.dbg.stop.is_some() {
            break;
        }
    }
    assert_eq!(m.dbg.take_stop(), Some(Stop::Count(5)));
    assert_eq!(m.dbg.instr, 5);
    // 5命令ぶんだけ進んでいる (NOPは1バイト)
    assert_eq!(m.cpu.ip, 0x7C05);
}

#[test]
fn 流し直せば必ず同じ場所に着く() {
    // **巻き戻しの根拠。** 決定的でなければ `goto` は嘘をつく。
    // 装置と割り込みを含めて同じにならなければ意味がないので、
    // BIOSのPOSTが済んだ実機同然の状態から回す
    let code = [
        0xB9, 0x00, 0x10, // MOV CX,0x1000
        0x49, // DEC CX
        0x75, 0xFD, // JNZ -3
        0xF4, // HLT
    ];
    let run_to = |n: u64| {
        let mut m = boot(&code);
        m.dbg.run_to(n);
        while m.dbg.stop.is_none() {
            m.step();
        }
        (m.cpu.regs, m.cpu.sregs, m.cpu.ip, m.cpu.eflags(), m.dbg.instr)
    };
    for n in [1u64, 100, 4096, 9000] {
        assert_eq!(run_to(n), run_to(n), "{n} 命令目が2回で違った");
    }
}

#[test]
fn 足跡は直近だけを残す() {
    let mut m = boot(&[0x90; 32]);
    m.dbg.record_trace(4);
    for _ in 0..10 {
        m.step();
    }
    assert_eq!(m.dbg.trace.len(), 4);
    // 最後の4命令が、順に並んでいる
    let ips: Vec<u32> = m.dbg.trace.iter().map(|s| s.ip).collect();
    assert_eq!(ips, vec![0x7C06, 0x7C07, 0x7C08, 0x7C09]);
    assert_eq!(m.dbg.trace.back().unwrap().bytes[0], 0x90);
}

#[test]
fn 見張りを外せば元締めも切れる() {
    let mut m = boot(&[0x90; 8]);
    m.dbg.break_at(0x7C00);
    m.dbg.watch_mem(0x0450);
    assert!(m.dbg.on);
    m.dbg.clear();
    assert!(!m.dbg.on);
    assert!(m.dbg.code.is_empty() && m.dbg.mem_write.is_empty());
}

#[test]
fn スナップショットにデバッガは入らない() {
    // デバッガは**観測する側**であって機械の状態ではない。
    // 保存して戻したときにブレークポイントまで戻ると、かえって驚く
    let mut m = boot(&[0x90; 8]);
    m.dbg.break_at(0x7C00);
    let saved = m.save_state();

    let mut n = boot(&[0x90; 8]);
    n.load_state(&saved).unwrap();
    assert!(n.dbg.code.is_empty());
    assert!(!n.dbg.on);
}
