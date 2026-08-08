//! シフトと回転 (GRP2)。8bit/16bit共通の実装。

use super::alu::{set_szp16, set_szp8};
use super::{Cpu, CF, OF};

// --- シフト/回転 (GRP2) ---
// 8086はカウントをマスクしないが、186以降 (およびUnicorn) は5bitでマスクする。
// 最終目標が32bit Linuxなので186以降の挙動に合わせる。
// カウント0のときはフラグを一切変更しない。AFは常に未定義。
pub fn shift_rot(c: &mut Cpu, kind: u8, val: u32, count_raw: u8, w: u32) -> u32 {
    let mask: u32 = if w == 8 { 0xFF } else { 0xFFFF };
    let count = (count_raw & 0x1F) as u32;
    if count == 0 {
        return val & mask;
    }
    let val = val & mask;
    let mut cf = c.flag(CF) as u32;
    let r: u32;
    match kind {
        0 => {
            // ROL
            let n = count % w;
            r = ((val << n) | (val >> ((w - n) % w))) & mask;
            cf = r & 1;
        }
        1 => {
            // ROR
            let n = count % w;
            r = ((val >> n) | (val << ((w - n) % w))) & mask;
            cf = (r >> (w - 1)) & 1;
        }
        2 => {
            // RCL (キャリーを含む w+1 bit の回転)
            let n = count % (w + 1);
            let mut x = val;
            for _ in 0..n {
                let newcf = (x >> (w - 1)) & 1;
                x = ((x << 1) | cf) & mask;
                cf = newcf;
            }
            r = x;
        }
        3 => {
            // RCR
            let n = count % (w + 1);
            let mut x = val;
            for _ in 0..n {
                let newcf = x & 1;
                x = (x >> 1) | (cf << (w - 1));
                cf = newcf;
            }
            r = x & mask;
        }
        4 | 6 => {
            // SHL / SAL
            cf = if count <= w {
                (val >> (w - count)) & 1
            } else {
                0
            };
            r = if count >= w { 0 } else { (val << count) & mask };
        }
        5 => {
            // SHR
            cf = if count <= w {
                (val >> (count - 1)) & 1
            } else {
                0
            };
            r = if count >= w { 0 } else { val >> count };
        }
        _ => {
            // SAR (符号を保つ)
            let sval = if w == 8 {
                val as u8 as i8 as i32
            } else {
                val as u16 as i16 as i32
            };
            let n = count.min(w - 1);
            cf = ((sval >> (count - 1).min(w - 1)) & 1) as u32;
            r = (sval >> n) as u32 & mask;
        }
    }
    c.set_flag(CF, cf != 0);
    // OFはカウント1のときのみ定義される
    if count == 1 {
        let msb = (r >> (w - 1)) & 1;
        let of = match kind {
            0 | 2 | 4 | 6 => msb ^ cf,           // 左回転・左シフト
            1 | 3 => msb ^ ((r >> (w - 2)) & 1), // 右回転
            5 => (val >> (w - 1)) & 1,           // SHR: 元のMSB
            _ => 0,                              // SAR
        };
        c.set_flag(OF, of != 0);
    }
    // 回転命令はSZPを変更しない
    if kind >= 4 {
        if w == 8 {
            set_szp8(c, r as u8);
        } else {
            set_szp16(c, r as u16);
        }
    }
    r
}
