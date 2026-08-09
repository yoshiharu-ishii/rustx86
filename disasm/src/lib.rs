//! 逆アセンブラ — 生バイトを人間が読めるニーモニックへ。
//!
//! **core には置かない。** coreは外部クレートに一切依存しない方針で、
//! 逆アセンブルは「画面に出すための都合」= 表示層の関心である。
//! iced-x86 という実績ある別の権威に任せることで、うちのデコーダとは
//! 独立した第二の実装になる (cosimがUnicornを借りるのと同じ構図)。
//!
//! 16bitコードと32bitコードで同じバイト列が別の命令になるので、
//! **ビット幅を呼び出し側が渡す** (CSのDビットが決める)。

use iced_x86::{Decoder, DecoderOptions, Formatter, GasFormatter};

/// バイト列の先頭1命令をニーモニックにする。`bits` は 16 か 32。
/// `ip` は表示上の番地 (相対分岐の解決に要る)
pub fn one(bytes: &[u8], bits: u32, ip: u64) -> String {
    let mut dec = Decoder::with_ip(bits, bytes, ip, DecoderOptions::NONE);
    if !dec.can_decode() {
        return "(no bytes)".into();
    }
    let insn = dec.decode();
    if insn.is_invalid() {
        return "(bad)".into();
    }
    let mut out = String::new();
    GasFormatter::new().format(&insn, &mut out);
    out
}

/// 先頭1命令のバイト数。デコードできなければ 0
pub fn len(bytes: &[u8], bits: u32) -> usize {
    let mut dec = Decoder::new(bits, bytes, DecoderOptions::NONE);
    if !dec.can_decode() {
        return 0;
    }
    let insn = dec.decode();
    if insn.is_invalid() {
        0
    } else {
        insn.len()
    }
}
