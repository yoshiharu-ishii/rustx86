//! Unicorn Engine (QEMUのCPU部分をライブラリ化したもの) をオラクルとした比較実行。
//!
//! 同じ初期状態 (レジスタ・フラグ・メモリ) を自作CPUとUnicornの両方に与えて
//! 1命令だけ実行し、実行後の状態を突き合わせる。食い違えば自作CPUのバグ。
//!
//! x86のEFLAGS意味論 (特にAF/OF/PFの境界条件) は手書きテストで網羅するのが
//! 非現実的なため、ランダム生成 + オラクル比較で機械的に潰す。
//!
//! 実行: `cargo test -p rustx86-cosim` (デフォルトビルド対象外)

use rustx86_core::cpu::{self, AX, BP, BX, CX, DI, DX, SI, SP};
use rustx86_core::Machine;

/// テストコードを置くアドレス (CS=0, IP=CODE_ADDR)
pub const CODE_ADDR: u16 = 0x1000;
/// 命令が読み書きするデータ領域 (DS:0x2000)
pub const DATA_ADDR: u16 = 0x2000;

/// 1ケース分の初期状態
#[derive(Clone, Debug)]
pub struct TestCase {
    pub code: Vec<u8>,
    pub regs: [u16; 8],
    pub flags: u16,
    /// DATA_ADDR に置く16バイト
    pub data: [u8; 16],
}

/// 実行後の観測状態
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct State {
    pub regs: [u16; 8],
    pub flags: u16,
    pub ip: u16,
    pub data: [u8; 16],
}

/// 比較対象のフラグ (x86が「未定義」と定めるものは呼び出し側でマスクする)
pub const FLAG_MASK_ALL: u16 = (cpu::CF | cpu::PF | cpu::AF | cpu::ZF | cpu::SF | cpu::OF) as u16;

pub fn flag_names(f: u16) -> String {
    let mut s = Vec::new();
    for (mask, name) in [
        (cpu::CF, "CF"),
        (cpu::PF, "PF"),
        (cpu::AF, "AF"),
        (cpu::ZF, "ZF"),
        (cpu::SF, "SF"),
        (cpu::OF, "OF"),
    ] {
        if f & mask as u16 != 0 {
            s.push(name);
        }
    }
    if s.is_empty() { "-".into() } else { s.join("|") }
}

/// 自作CPUで1命令実行する
pub fn run_ours(tc: &TestCase) -> State {
    let mut m = Machine::new();
    for (i, b) in tc.code.iter().enumerate() {
        m.write8(CODE_ADDR as u32 + i as u32, *b);
    }
    for (i, b) in tc.data.iter().enumerate() {
        m.write8(DATA_ADDR as u32 + i as u32, *b);
    }
    m.cpu.regs[..8].copy_from_slice(&tc.regs.map(|v| v as u32));
    m.cpu.flags = tc.flags as u32 | 0x0002;
    m.cpu.set_cs_ip(0, CODE_ADDR);
    m.step();
    let mut data = [0u8; 16];
    for (i, d) in data.iter_mut().enumerate() {
        *d = m.read8(DATA_ADDR as u32 + i as u32);
    }
    State {
        regs: std::array::from_fn(|i| m.cpu.regs[i] as u16),
        flags: m.cpu.flags as u16 & FLAG_MASK_ALL,
        ip: m.cpu.ip,
        data,
    }
}

/// レジスタ名 (エラー表示用)
pub const REG_NAMES: [&str; 8] = ["AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI"];

pub const REG_ORDER: [usize; 8] = [AX, CX, DX, BX, SP, BP, SI, DI];

/// 2つの状態の差分を人間可読な文字列で返す (一致ならNone)
pub fn diff(ours: &State, oracle: &State, flag_mask: u16) -> Option<String> {
    let mut out = Vec::new();
    for i in 0..8 {
        if ours.regs[i] != oracle.regs[i] {
            out.push(format!(
                "{}: ours={:04x} oracle={:04x}",
                REG_NAMES[i], ours.regs[i], oracle.regs[i]
            ));
        }
    }
    let fo = ours.flags & flag_mask;
    let fu = oracle.flags & flag_mask;
    if fo != fu {
        out.push(format!(
            "FLAGS: ours={} oracle={} (差分 {})",
            flag_names(fo),
            flag_names(fu),
            flag_names(fo ^ fu)
        ));
    }
    if ours.ip != oracle.ip {
        out.push(format!("IP: ours={:04x} oracle={:04x}", ours.ip, oracle.ip));
    }
    if ours.data != oracle.data {
        out.push(format!(
            "MEM: ours={:02x?} oracle={:02x?}",
            ours.data, oracle.data
        ));
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n  "))
    }
}

/// 決定的な擬似乱数 (xorshift64*)。テストの再現性のため外部クレートを使わない
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_u16(&mut self) -> u16 {
        self.next_u64() as u16
    }

    /// 境界値を混ぜた8bit値 (0, 1, 0x0F, 0x10, 0x7F, 0x80, 0xFF を高頻度で出す)
    pub fn interesting_u8(&mut self) -> u8 {
        const EDGE: [u8; 8] = [0x00, 0x01, 0x0F, 0x10, 0x7F, 0x80, 0xFE, 0xFF];
        let r = self.next_u64();
        if r % 2 == 0 {
            EDGE[(r >> 8) as usize % EDGE.len()]
        } else {
            r as u8
        }
    }

    pub fn interesting_u16(&mut self) -> u16 {
        const EDGE: [u16; 8] = [0x0000, 0x0001, 0x000F, 0x0010, 0x7FFF, 0x8000, 0xFFFE, 0xFFFF];
        let r = self.next_u64();
        if r % 2 == 0 {
            EDGE[(r >> 8) as usize % EDGE.len()]
        } else {
            r as u16
        }
    }
}
