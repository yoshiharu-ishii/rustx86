//! Adlib / OPL2 (Yamaha YM3812) — FM 音源。ポート 0x388 (index / status) と 0x389 (data)。
//!
//! DOOM の音楽はこれで鳴る (DMX が MUS を OPL のレジスタ書きに直す)。DMA も IRQ も
//! 要らず、ポート 2 本で完結する — Tier 6 の「音付き DOOM」に最短の装置 (6t)。
//!
//! ## 二つの役
//!
//! 1. **レジスタとタイマ** — ゲストから見える顔。DOOM の Adlib 検出は
//!    「タイマ1 を 0xFF で走らせ、status の bit7 (IRQ) と bit6 (T1) が立つか」なので、
//!    タイマは本物どおり 80µs / 320µs 刻みで満了させる。時計は機械の `tsc`
//!    (1 命令 ≒ 一定時間) から**遅延評価** — tick の配線を足さない
//! 2. **合成器** — 9 チャネル × 2 オペレータの FM。サイン波 4 種、ADSR、
//!    キースケーリング、フィードバック、AM/VIB の LFO。出力は指定サンプルレートの
//!    i16 モノラル。**ゲストの状態には一切影響しない** (決定性の外側) ので、
//!    描き手 (JS) が好きな時に好きな量だけ引き出す。
//!
//! 合成は YM3812 の回路の写しではなく「楽譜どおりの音が出る」近似。
//! 周波数・ADSR の段・波形・接続は本物の式で、エンベロープの速さは
//! 既知の実測値に合わせた近似 (既存の OPL コアは GPL/LGPL なので持ち込まない)。

use core::f32::consts::PI;

/// タイマ 1 の 1 刻み (80µs) の命令数。PIT 1.193182MHz × 64 命令/クロック ≒ 76.4 命令/µs
const T1_TICK_INSTRS: u64 = 6109;
/// タイマ 2 は 320µs 刻み
const T2_TICK_INSTRS: u64 = T1_TICK_INSTRS * 4;

/// OPL のマスタークロック由来の基準レート (F-Number の式に使う)
const OPL_RATE: f32 = 49716.0;

/// オペレータの並び: チャネル ch のモジュレータ = OP_OFFSET[ch]、キャリア = +3
/// (レジスタの下位 5bit は 0-5, 8-13, 16-21 の飛び番地)
const CH_MOD: [usize; 9] = [0, 1, 2, 8, 9, 10, 16, 17, 18];
const MULT: [f32; 16] = [
    0.5, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 10.0, 12.0, 12.0, 15.0, 15.0,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Op {
    // レジスタの写し
    am: bool,
    vib: bool,
    egt: bool, // sustain を保つ (true) か、sustain 段も減衰し続ける (false)
    ksr: bool,
    mult: f32,
    ksl: u8,
    tl: u8, // 0.75dB 刻み
    ar: u8,
    dr: u8,
    sl: u8, // 3dB 刻み (15 = -93dB)
    rr: u8,
    wave: u8,
    // 走っている状態
    phase: f32, // 0..1
    env: f32,   // 減衰量 (dB、0 = 最大音量、96 = 無音)
    stage: Stage,
    out: f32, // 直前の出力 (フィードバック用)
    out_prev: f32,
}

impl Op {
    const fn new() -> Self {
        Self {
            am: false,
            vib: false,
            egt: false,
            ksr: false,
            mult: 1.0,
            ksl: 0,
            tl: 63,
            ar: 0,
            dr: 0,
            sl: 0,
            rr: 0,
            wave: 0,
            phase: 0.0,
            env: 96.0,
            stage: Stage::Off,
            out: 0.0,
            out_prev: 0.0,
        }
    }
}

#[derive(Clone, Copy)]
struct Ch {
    fnum: u16,
    block: u8,
    key: bool,
    fb: u8,
    additive: bool,
}

pub struct Opl2 {
    index: u8,
    regs: [u8; 256],
    ops: [Op; 22],
    chs: [Ch; 9],
    /// タイマ: 起動時の (カウント初期値, 起動時刻 tsc)。None = 止まっている
    t1: Option<(u8, u64)>,
    t2: Option<(u8, u64)>,
    /// 満了の印 (status の bit6/bit5)。reset (reg 4 bit7) で消える
    t1_flag: bool,
    t2_flag: bool,
    t1_mask: bool,
    t2_mask: bool,
    /// 合成のサンプルレート (描き手が決める。既定 49716)
    rate: f32,
    lfo_am: f32,
    lfo_vib: f32,
    am_depth: bool,
    vib_depth: bool,
    /// ポートを触った回数。**ISA バスの時間**の代わり — 実機では 1 回の IN/OUT に
    /// ~1µs かかり、DMX の Adlib 検出は「status を数十回読む」ことで 80µs を待つ。
    /// 命令数だけの時計ではその待ちが 800 命令 (~10µs) にしかならず、タイマが
    /// 満了しないまま「Adlib isn't responding」になった (2026-08-22)
    io_ticks: u64,
    /// key-on の回数 (診断・テスト: 曲が鳴っているか)
    pub key_ons: u64,
    /// レジスタ書き込みの回数 (診断)
    pub writes: u64,
}

impl Default for Opl2 {
    fn default() -> Self {
        Self::new()
    }
}

impl Opl2 {
    pub fn new() -> Self {
        Self {
            index: 0,
            regs: [0; 256],
            ops: [Op::new(); 22],
            chs: [Ch {
                fnum: 0,
                block: 0,
                key: false,
                fb: 0,
                additive: false,
            }; 9],
            t1: None,
            t2: None,
            t1_flag: false,
            t2_flag: false,
            t1_mask: false,
            t2_mask: false,
            rate: OPL_RATE,
            lfo_am: 0.0,
            lfo_vib: 0.0,
            am_depth: false,
            vib_depth: false,
            io_ticks: 0,
            key_ons: 0,
            writes: 0,
        }
    }

    /// 合成のサンプルレートを決める (AudioContext の rate に合わせる)
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(8000) as f32;
    }

    // ---- ゲストから見える顔 ----

    pub fn write_index(&mut self, v: u8) {
        self.io_ticks += 1;
        self.index = v;
    }

    /// status (0x388 の読み): bit7 = いずれかのタイマが満了 (マスク外)、bit6 = T1、bit5 = T2。
    /// `now` は機械の tsc — タイマの満了はここで初めて評価する
    /// 機械の tsc に ISA バスの時間 (ポート 1 回 ≒ 1µs ≒ 76 命令) を足した時計
    fn clock(&self, tsc: u64) -> u64 {
        tsc.wrapping_add(self.io_ticks * 76)
    }

    pub fn status(&mut self, now: u64) -> u8 {
        self.io_ticks += 1;
        let now = self.clock(now);
        self.poll_timers(now);
        let mut s = 0u8;
        if self.t1_flag && !self.t1_mask {
            s |= 0x40;
        }
        if self.t2_flag && !self.t2_mask {
            s |= 0x20;
        }
        if s != 0 {
            s |= 0x80;
        }
        // 下位 bit は YM3812 では 0 (OPL3 は bit1-2 に 0 が入り、bit0 が 0)。
        // 検出コードは上位 3bit しか見ない
        s
    }

    fn poll_timers(&mut self, now: u64) {
        if let Some((count, start)) = self.t1 {
            let need = (256 - count as u64) * T1_TICK_INSTRS;
            if now.wrapping_sub(start) >= need {
                self.t1_flag = true;
                self.t1 = Some((count, now)); // 周期的に鳴り続ける (再装填)
            }
        }
        if let Some((count, start)) = self.t2 {
            let need = (256 - count as u64) * T2_TICK_INSTRS;
            if now.wrapping_sub(start) >= need {
                self.t2_flag = true;
                self.t2 = Some((count, now));
            }
        }
    }

    pub fn write_data(&mut self, v: u8, now: u64) {
        self.writes += 1;
        self.io_ticks += 1;
        let now = self.clock(now);
        let r = self.index;
        self.regs[r as usize] = v;
        match r {
            0x02 | 0x03 => {} // カウント初期値。起動時 (reg 4) に読む
            0x04 => {
                if v & 0x80 != 0 {
                    // IRQ リセット: 満了の印を消す。他のビットは無視される
                    self.t1_flag = false;
                    self.t2_flag = false;
                    return;
                }
                self.t1_mask = v & 0x40 != 0;
                self.t2_mask = v & 0x20 != 0;
                self.t1 = if v & 0x01 != 0 {
                    Some((self.regs[0x02], now))
                } else {
                    None
                };
                self.t2 = if v & 0x02 != 0 {
                    Some((self.regs[0x03], now))
                } else {
                    None
                };
            }
            0x20..=0x35 | 0x40..=0x55 | 0x60..=0x75 | 0x80..=0x95 | 0xE0..=0xF5 => {
                let slot = (r & 0x1F) as usize;
                if slot >= 22 || !matches!(slot % 8, 0..=5) {
                    return;
                }
                let op = &mut self.ops[slot];
                match r & 0xE0 {
                    0x20 => {
                        op.am = v & 0x80 != 0;
                        op.vib = v & 0x40 != 0;
                        op.egt = v & 0x20 != 0;
                        op.ksr = v & 0x10 != 0;
                        op.mult = MULT[(v & 0x0F) as usize];
                    }
                    0x40 => {
                        op.ksl = v >> 6;
                        op.tl = v & 0x3F;
                    }
                    0x60 => {
                        op.ar = v >> 4;
                        op.dr = v & 0x0F;
                    }
                    0x80 => {
                        op.sl = v >> 4;
                        op.rr = v & 0x0F;
                    }
                    _ => {
                        // 0xE0: 波形 (bit0-1)。WSE (reg 1 bit5) が 0 なら無視される
                        op.wave = if self.regs[0x01] & 0x20 != 0 {
                            v & 0x03
                        } else {
                            0
                        };
                    }
                }
            }
            0xA0..=0xA8 => {
                let ch = &mut self.chs[(r - 0xA0) as usize];
                ch.fnum = (ch.fnum & 0x300) | v as u16;
            }
            0xB0..=0xB8 => {
                let i = (r - 0xB0) as usize;
                let ch = &mut self.chs[i];
                ch.fnum = (ch.fnum & 0xFF) | (((v & 0x03) as u16) << 8);
                ch.block = (v >> 2) & 0x07;
                let key = v & 0x20 != 0;
                if key && !ch.key {
                    self.key_ons += 1;
                    for s in [CH_MOD[i], CH_MOD[i] + 3] {
                        let op = &mut self.ops[s];
                        op.phase = 0.0;
                        op.stage = Stage::Attack;
                    }
                } else if !key && ch.key {
                    for s in [CH_MOD[i], CH_MOD[i] + 3] {
                        self.ops[s].stage = Stage::Release;
                    }
                }
                ch.key = key;
            }
            0xBD => {
                self.am_depth = v & 0x80 != 0;
                self.vib_depth = v & 0x40 != 0;
                // リズムモード (bit5) と打楽器のキーは未対応 — DOOM の音楽は
                // メロディモードで鳴る (台帳)
            }
            0xC0..=0xC8 => {
                let ch = &mut self.chs[(r - 0xC0) as usize];
                ch.fb = (v >> 1) & 0x07;
                ch.additive = v & 0x01 != 0;
            }
            _ => {}
        }
    }

    // ---- 合成 ----

    /// KSR: ブロックと F-Number の上位で速さに下駄を履かせる
    fn eff_rate(op: &Op, base: u8, block: u8, fnum: u16) -> u8 {
        if base == 0 {
            return 0;
        }
        let ks = (block << 1) | ((fnum >> 9) & 1) as u8; // 0..15
        let add = if op.ksr { ks } else { ks >> 2 };
        (base * 4 + add).min(63)
    }

    /// 1 サンプル進める (エンベロープと位相)。出力は -1..1
    fn step_op(&mut self, slot: usize, ch: usize, modulation: f32) -> f32 {
        let rate = self.rate;
        let (block, fnum) = (self.chs[ch].block, self.chs[ch].fnum);
        let am_depth = self.am_depth;
        let vib_depth = self.vib_depth;
        let (lfo_am, lfo_vib) = (self.lfo_am, self.lfo_vib);
        let fb = self.chs[ch].fb;
        let op = &mut self.ops[slot];
        // エンベロープ
        match op.stage {
            Stage::Off => return 0.0,
            Stage::Attack => {
                let r = Self::eff_rate(op, op.ar, block, fnum);
                if r >= 60 {
                    op.env = 0.0;
                } else {
                    // アタックは指数的に 0dB へ寄る (本物の形)
                    let step = 96.0 / (10.0 * 2f32.powf(-((r as f32 - 4.0) / 4.0)) * rate);
                    op.env -= step * (op.env / 96.0 * 3.0 + 0.25);
                }
                if op.env <= 0.0 {
                    op.env = 0.0;
                    op.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                let r = Self::eff_rate(op, op.dr, block, fnum);
                let sl = if op.sl == 15 {
                    93.0
                } else {
                    op.sl as f32 * 3.0
                };
                op.env += {
                    let secs = 10.0 * 2f32.powf(-((r as f32 - 4.0) / 4.0));
                    if r == 0 {
                        0.0
                    } else {
                        96.0 / (secs * rate)
                    }
                };
                if op.env >= sl {
                    op.env = sl;
                    op.stage = Stage::Sustain;
                }
            }
            Stage::Sustain => {
                if !op.egt {
                    // sustain を保たない: そのまま RR で減衰し続ける
                    let r = Self::eff_rate(op, op.rr, block, fnum);
                    if r > 0 {
                        let secs = 10.0 * 2f32.powf(-((r as f32 - 4.0) / 4.0));
                        op.env += 96.0 / (secs * rate);
                    }
                }
            }
            Stage::Release => {
                let r = Self::eff_rate(op, op.rr, block, fnum);
                if r > 0 {
                    let secs = 10.0 * 2f32.powf(-((r as f32 - 4.0) / 4.0));
                    op.env += 96.0 / (secs * rate);
                }
            }
        }
        if op.env >= 96.0 {
            op.env = 96.0;
            op.stage = Stage::Off;
            op.out = 0.0;
            op.out_prev = 0.0;
            return 0.0;
        }
        // 位相: f = fnum × 2^block × OPL_RATE / 2^20 × MULT (+ ビブラート)
        let mut freq =
            fnum as f32 * (1u32 << block) as f32 * OPL_RATE / (1u32 << 20) as f32 * op.mult;
        if op.vib {
            let depth = if vib_depth { 0.014 } else { 0.007 };
            freq *= 1.0 + lfo_vib * depth;
        }
        op.phase += freq / rate;
        if op.phase >= 1.0 {
            op.phase -= op.phase.floor();
        }
        // 音量: TL (0.75dB 刻み) + エンベロープ + トレモロ
        let mut att_db = op.tl as f32 * 0.75 + op.env;
        if op.am {
            att_db += (1.0 - lfo_am) * if am_depth { 4.8 } else { 1.0 } / 2.0;
        }
        let amp = 10f32.powf(-att_db / 20.0);
        // 波形 (位相 + 変調 + 自己フィードバック)
        let fbk = if fb > 0 && slot % 8 < 3 {
            // フィードバックはモジュレータだけ: 直前 2 出力の平均 × 2^(fb-1)/... (本物の比率)
            (op.out + op.out_prev) * 0.5 * (1u32 << (fb - 1)) as f32 / 16.0
        } else {
            0.0
        };
        let ph = op.phase + modulation + fbk;
        let s = (ph * 2.0 * PI).sin();
        let w = match op.wave {
            0 => s,
            1 => s.max(0.0),
            2 => s.abs(),
            _ => {
                let q = ph - ph.floor();
                if q < 0.25 || (0.5..0.75).contains(&q) {
                    s.abs()
                } else {
                    0.0
                }
            }
        };
        let out = w * amp;
        op.out_prev = op.out;
        op.out = out;
        out
    }

    /// `n` サンプルぶん合成して i16 で返す (モノラル)
    pub fn render(&mut self, n: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(n);
        let rate = self.rate;
        for _ in 0..n {
            // LFO: トレモロ 3.7Hz、ビブラート 6.1Hz
            self.lfo_am = (self.lfo_am + 3.7 / rate).fract();
            self.lfo_vib = (self.lfo_vib + 6.1 / rate).fract();
            let am = ((self.lfo_am * 2.0 * PI).sin() + 1.0) * 0.5;
            let vib = (self.lfo_vib * 2.0 * PI).sin();
            let (save_am, save_vib) = (self.lfo_am, self.lfo_vib);
            self.lfo_am = am;
            self.lfo_vib = vib;
            let mut mix = 0.0f32;
            for (ch, &m) in CH_MOD.iter().enumerate() {
                let c = m + 3;
                if self.ops[m].stage == Stage::Off && self.ops[c].stage == Stage::Off {
                    continue;
                }
                let modv = self.step_op(m, ch, 0.0);
                if self.chs[ch].additive {
                    let carrier = self.step_op(c, ch, 0.0);
                    mix += modv + carrier;
                } else {
                    // FM: モジュレータの出力で位相を振る (本物の変調指数に寄せる)
                    mix += self.step_op(c, ch, modv * 0.5);
                }
            }
            self.lfo_am = save_am;
            self.lfo_vib = save_vib;
            // 9ch 合成の余裕を見て 1/4 に落とし、飽和させる
            let v = (mix * 0.25 * 32767.0).clamp(-32768.0, 32767.0);
            out.push(v as i16);
        }
        out
    }

    /// 何か鳴っているか (テスト・診断)
    pub fn any_key_on(&self) -> bool {
        self.chs.iter().any(|c| c.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DOOM (DMX) の Adlib 検出: タイマ1 を 0xFF で走らせると 80µs 後に status が 0xC0
    #[test]
    fn timer_detection_like_dmx() {
        let mut o = Opl2::new();
        o.write_index(0x04);
        o.write_data(0x60, 0); // マスク
        o.write_index(0x04);
        o.write_data(0x80, 0); // IRQ リセット
        assert_eq!(o.status(0) & 0xE0, 0x00);
        o.write_index(0x02);
        o.write_data(0xFF, 0);
        o.write_index(0x04);
        o.write_data(0x21, 0); // T1 start、T2 mask
        assert_eq!(o.status(100) & 0xE0, 0x00, "まだ 80µs 経っていない");
        assert_eq!(o.status(T1_TICK_INSTRS + 1) & 0xE0, 0xC0, "満了");
        // DMX 流: status を読むだけで待つ (ISA バスの時間で満了する)
        let mut o2 = Opl2::new();
        o2.write_index(0x02);
        o2.write_data(0xFF, 0);
        o2.write_index(0x04);
        o2.write_data(0x21, 0);
        let mut st = 0;
        for _ in 0..100 {
            st = o2.status(0);
        }
        assert_eq!(st & 0xE0, 0xC0, "ポート読みの時間で満了する");
        o.write_index(0x04);
        o.write_data(0x60, 100_000);
        o.write_index(0x04);
        o.write_data(0x80, 100_000);
        assert_eq!(o.status(200_000) & 0xE0, 0x00, "リセット後は消える");
    }

    /// key-on で音が出て、key-off + リリースで消える
    #[test]
    fn key_on_makes_sound_and_release_fades() {
        let mut o = Opl2::new();
        o.set_rate(44100);
        let w = |o: &mut Opl2, r: u8, v: u8| {
            o.write_index(r);
            o.write_data(v, 0);
        };
        // ch0: モジュレータ (slot 0) は黙らせ、キャリア (slot 3) をサイン波で
        w(&mut o, 0x20, 0x01); // mult 1
        w(&mut o, 0x40, 0x3F); // モジュレータ TL 最大 (無音)
        w(&mut o, 0x23, 0x01);
        w(&mut o, 0x43, 0x00); // キャリア TL 0
        w(&mut o, 0x63, 0xF4); // AR 15, DR 4
        w(&mut o, 0x83, 0x2A); // SL 2, RR 10
        w(&mut o, 0xA0, 0x98); // F-Number (A4 付近: block 4, fnum 0x298)
        w(&mut o, 0xB0, 0x20 | (4 << 2) | 0x02); // key on
        let s = o.render(4410);
        let rms = (s.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
        assert!(rms > 500.0, "鳴っていない rms={rms}");
        assert_eq!(o.key_ons, 1);
        w(&mut o, 0xB0, (4 << 2) | 0x02); // key off
        let _ = o.render(44100); // 1 秒のリリース
        let s2 = o.render(4410);
        let rms2 = (s2.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / s2.len() as f64).sqrt();
        assert!(rms2 < rms * 0.05, "消えていない rms2={rms2}");
    }
}
