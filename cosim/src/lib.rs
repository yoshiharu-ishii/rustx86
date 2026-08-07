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

/// スタック観測窓の先頭 (SS=0)。この32バイトを突き合わせる
pub const STACK_BASE: u16 = 0x2FE0;
/// スタック命令のテンプレートが使う初期SP。
/// 窓の中央に置き、PUSH側 (下へ伸びる) もPOP側 (上を読む) も窓に収まるようにする。
/// PUSHA (16バイト) でも STACK_BASE を割らない
pub const STACK_INIT_SP: u16 = 0x2FF8;
/// スタック観測窓のバイト数
pub const STACK_WINDOW: usize = 32;

/// 1ケース分の初期状態
#[derive(Clone, Debug)]
pub struct TestCase {
    pub code: Vec<u8>,
    pub regs: [u16; 8],
    /// ES CS SS DS。CS/SS/DSは0固定 (コード・スタック・データの配置を単純に保つ)。
    /// ESだけは自由に振れるので PUSH ES / ストリング命令の宛先を検証できる
    pub sregs: [u16; 4],
    pub flags: u16,
    /// DATA_ADDR に置く16バイト
    pub data: [u8; 16],
    /// STACK_BASE に置く32バイト。POP系の入力を意味のある値にする
    pub stack: [u8; STACK_WINDOW],
}

/// 実行後の観測状態
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct State {
    pub regs: [u16; 8],
    /// ES CS SS DS。far転送・POP Sreg・LES/LDS はここに出る
    pub sregs: [u16; 4],
    pub flags: u16,
    pub ip: u16,
    pub data: [u8; 16],
    pub stack: [u8; STACK_WINDOW],
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
    for (i, b) in tc.stack.iter().enumerate() {
        m.write8(STACK_BASE as u32 + i as u32, *b);
    }
    m.cpu.regs[..8].copy_from_slice(&tc.regs.map(|v| v as u32));
    m.cpu.sregs[..4].copy_from_slice(&tc.sregs);
    m.cpu.flags = tc.flags as u32 | 0x0002;
    m.cpu.set_cs_ip(0, CODE_ADDR);
    m.step();
    let mut data = [0u8; 16];
    for (i, d) in data.iter_mut().enumerate() {
        *d = m.read8(DATA_ADDR as u32 + i as u32);
    }
    let mut stack = [0u8; STACK_WINDOW];
    for (i, d) in stack.iter_mut().enumerate() {
        *d = m.read8(STACK_BASE as u32 + i as u32);
    }
    State {
        regs: std::array::from_fn(|i| m.cpu.regs[i] as u16),
        sregs: std::array::from_fn(|i| m.cpu.sregs[i]),
        flags: m.cpu.flags as u16 & FLAG_MASK_ALL,
        ip: m.cpu.ip,
        data,
        stack,
    }
}

/// レジスタ名 (エラー表示用)
pub const REG_NAMES: [&str; 8] = ["AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI"];

pub const REG_ORDER: [usize; 8] = [AX, CX, DX, BX, SP, BP, SI, DI];

/// セグメントレジスタ名 (Cpu::sregs の並び順)
pub const SREG_NAMES: [&str; 4] = ["ES", "CS", "SS", "DS"];

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
    for (i, name) in SREG_NAMES.iter().enumerate() {
        if ours.sregs[i] != oracle.sregs[i] {
            out.push(format!(
                "{}: ours={:04x} oracle={:04x}",
                name, ours.sregs[i], oracle.sregs[i]
            ));
        }
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
    if ours.stack != oracle.stack {
        out.push(format!(
            "STACK: ours={:02x?} oracle={:02x?}",
            ours.stack, oracle.stack
        ));
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n  "))
    }
}

// x86が「未定義」と定めるフラグ (比較から除外する)。
// 未定義とは「どんな値でもよい」の意で、実CPUごとに違う値が入りうる。
pub const UD_CF: u16 = 0x0001;
pub const UD_PF: u16 = 0x0004;
pub const UD_AF: u16 = 0x0010;
pub const UD_ZF: u16 = 0x0040;
pub const UD_SF: u16 = 0x0080;
pub const UD_OF: u16 = 0x0800;

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

// ============ Unicorn オラクルと検証ハーネス ============

use unicorn_engine::unicorn_const::{Arch, Mode, Prot};
use unicorn_engine::{RegisterX86, Unicorn};

/// Unicornで1命令実行してオラクルの状態を得る
pub fn run_oracle(tc: &TestCase) -> State {
    let mut uc = Unicorn::new(Arch::X86, Mode::MODE_16).expect("unicorn init");
    uc.mem_map(0, 0x100000, Prot::ALL).expect("map");
    uc.mem_write(CODE_ADDR as u64, &tc.code).expect("write code");
    uc.mem_write(DATA_ADDR as u64, &tc.data).expect("write data");
    uc.mem_write(STACK_BASE as u64, &tc.stack).expect("write stack");

    let regs = [
        RegisterX86::AX,
        RegisterX86::CX,
        RegisterX86::DX,
        RegisterX86::BX,
        RegisterX86::SP,
        RegisterX86::BP,
        RegisterX86::SI,
        RegisterX86::DI,
    ];
    for (i, r) in regs.iter().enumerate() {
        uc.reg_write(*r, tc.regs[i] as u64).expect("reg");
    }
    // sregs の並びは Cpu::sregs と同じ ES CS SS DS
    let sregs = [
        RegisterX86::ES,
        RegisterX86::CS,
        RegisterX86::SS,
        RegisterX86::DS,
    ];
    for (i, s) in sregs.iter().enumerate() {
        uc.reg_write(*s, tc.sregs[i] as u64).expect("sreg");
    }
    uc.reg_write(RegisterX86::EFLAGS, tc.flags as u64 | 0x0002)
        .expect("flags");

    // 1命令だけ実行 (until=0で無効化し、count=1で止める)
    uc.emu_start(CODE_ADDR as u64, 0xFFFF, 0, 1).expect("emu");

    let mut out_regs = [0u16; 8];
    for (i, r) in regs.iter().enumerate() {
        out_regs[i] = uc.reg_read(*r).expect("read reg") as u16;
    }
    let mut out_sregs = [0u16; 4];
    for (i, s) in sregs.iter().enumerate() {
        out_sregs[i] = uc.reg_read(*s).expect("read sreg") as u16;
    }
    let flags = uc.reg_read(RegisterX86::EFLAGS).expect("read flags") as u16;
    let ip = uc.reg_read(RegisterX86::IP).expect("read ip") as u16;
    let mut data = [0u8; 16];
    uc.mem_read(DATA_ADDR as u64, &mut data).expect("read data");
    let mut stack = [0u8; STACK_WINDOW];
    uc.mem_read(STACK_BASE as u64, &mut stack).expect("read stack");

    State {
        regs: out_regs,
        sregs: out_sregs,
        flags: flags & FLAG_MASK_ALL,
        ip,
        data,
        stack,
    }
}


/// 明示的に構築したケース列を検証する (状態空間が小さい命令の総当たり用)
pub fn check_cases(name: &str, undefined: u16, cases: Vec<TestCase>) {
    let total = cases.len();
    for tc in cases {
        let ours = run_ours(&tc);
        let oracle = run_oracle(&tc);
        if let Some(d) = diff(&ours, &oracle, FLAG_MASK_ALL & !undefined) {
            panic!(
                "co-sim mismatch [{name}] code={:02x?} AX={:04x} flags_in={}\n  {}",
                tc.code,
                tc.regs[0],
                flag_names(tc.flags),
                d
            );
        }
    }
    eprintln!("{name}: {total} cases (exhaustive)");
}

/// AL/AH/フラグの全組み合わせを総当たりするケース列
pub fn sweep_al(code: Vec<u8>) -> Vec<TestCase> {
    let mut out = Vec::new();
    for al in 0..=255u16 {
        for ah in [0x00u16, 0x12, 0x99] {
            for f in [0u16, 0x0001, 0x0010, 0x0011] {
                out.push(TestCase {
                    code: code.clone(),
                    regs: [(ah << 8) | al, 0x1234, 0x5678, 0x9ABC, 0x0100, 0x0200, 0x0300, 0x0400],
                    sregs: [0; 4],
                    flags: f,
                    data: [0; 16],
                    stack: [0; STACK_WINDOW],
                });
            }
        }
    }
    out
}

/// 命令テンプレート: ランダム値から命令バイト列を組み立てる
pub struct Template {
    pub name: &'static str,
    /// 比較から除外するフラグ (x86が未定義と定めるもの)
    pub undefined: u16,
    pub build: fn(&mut Rng) -> Vec<u8>,
    /// レジスタ初期値の補正 (DIVのゼロ除算・商オーバーフローを避けるなど)
    pub fixup: fn(&mut [u16; 8]),
}

pub fn nofix(_: &mut [u16; 8]) {}

/// スタックに触る命令用の fixup。SPを観測窓の中に固定する。
///
/// SPをランダムのままにすると、PUSHの書き込み先がコード領域に当たった際に
/// QEMU (Unicorn) の自己書き換えコード検出が働き、命令をやり直してしまう。
/// 差分が出ても自作CPUのバグではない — オラクル都合の偽陽性になる。
pub fn fix_stack(regs: &mut [u16; 8]) {
    regs[SP_IDX] = STACK_INIT_SP;
}

/// regs配列でのSPの位置 (AX CX DX BX SP BP SI DI)
pub const SP_IDX: usize = 4;
/// regs配列でのBPの位置
pub const BP_IDX: usize = 5;

/// POPA用。16バイト読むので、SPを窓の底に置いて読み出しが窓を出ないようにする
pub fn fix_stack_low(regs: &mut [u16; 8]) {
    regs[SP_IDX] = STACK_BASE;
}

/// ENTER/LEAVE用。スタックフレーム命令はBPも参照するので両方を窓に入れる
pub fn fix_frame(regs: &mut [u16; 8]) {
    regs[SP_IDX] = STACK_INIT_SP;
    regs[BP_IDX] = STACK_INIT_SP;
}

/// XLAT用。BXを変換テーブルの先頭に固定する (AL は 0-255 のまま振る)
pub fn fix_xlat(regs: &mut [u16; 8]) {
    regs[3] = DATA_ADDR; // BX
}

pub fn random_case(rng: &mut Rng, t: &Template) -> TestCase {
    let mut regs: [u16; 8] = std::array::from_fn(|_| rng.interesting_u16());
    (t.fixup)(&mut regs);
    TestCase {
        code: (t.build)(rng),
        regs,
        // CS/SS/DS は0固定 (コード・スタック・データの配置を単純に保つ)。
        // ESだけ振り、PUSH ES / POP ES / LES / ストリング命令の宛先を検証する。
        //
        // ESを 0x0FFF までに抑えるのは、ストリング命令の書き込み先 ES:DI が
        // 1MBを超えるとオラクル (Unicorn) が未マップ領域に書いて落ちるため。
        // 自作CPU側は linear() が 20bit でラップするので落ちない。
        // この非対称は A20 ゲートの話であり、Tier 4 で正面から扱う
        sregs: [rng.interesting_u16() & 0x0FFF, 0, 0, 0],
        flags: (rng.next_u16() & (FLAG_MASK_ALL | 0x0400)),
        data: std::array::from_fn(|_| rng.interesting_u8()),
        // スタックに積んでおく値は「上位バイトを 0x00-0x0F に抑える」。
        // RETF/IRET が pop した値はそのままCSに入るため、CS:IP が 1MB を
        // 超えるとオラクル (Unicorn) が未マップ領域を触って落ちる。
        // 自作CPU側は linear() が 20bit でラップするので落ちない —
        // この非対称は A20 ゲートの話であり、Tier 4 で正面から扱う
        stack: std::array::from_fn(|i| {
            if i % 2 == 1 {
                rng.next_u16() as u8 & 0x0F
            } else {
                rng.interesting_u8()
            }
        }),
    }
}

pub fn check(templates: &[Template], cases_per_template: usize, seed: u64) {
    let mut failures = Vec::new();
    for t in templates {
        let mut rng = Rng::new(seed ^ t.name.len() as u64 * 0x9E37_79B9);
        let mut checked = 0;
        for _ in 0..cases_per_template {
            let tc = random_case(&mut rng, t);
            let ours = run_ours(&tc);
            let oracle = run_oracle(&tc);
            checked += 1;
            if let Some(d) = diff(&ours, &oracle, FLAG_MASK_ALL & !t.undefined) {
                failures.push(format!(
                    "[{}] code={:02x?} regs={:04x?} flags_in={}\n  {}",
                    t.name,
                    tc.code,
                    tc.regs,
                    flag_names(tc.flags),
                    d
                ));
                break; // テンプレートごとに最初の1件だけ報告
            }
        }
        eprintln!("{}: {checked} cases", t.name);
    }
    assert!(
        failures.is_empty(),
        "co-sim mismatch ({} template(s)):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

