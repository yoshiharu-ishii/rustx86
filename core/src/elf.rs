//! 非圧縮カーネル (vmlinux, ELF32) を読むための最小のELFパーサ。
//!
//! ## なぜ vmlinux を直接ロードするのか
//!
//! bzImage は「圧縮カーネル + 自己解凍ステブ」で、実行するとゲストの中で
//! 解凍が走る。実測で**起動全体の55% (540M命令) がこの解凍**であり、しかも
//! シリアルに何も出せない「無言の黒画面」になる。展開済みの vmlinux を
//! ホスト側でロードすれば、この区間は丸ごと消える。
//! Firecracker が bzImage ではなく vmlinux を要求するのは、まさにこのため。
//!
//! ## 実装の範囲
//!
//! 汎用のELFローダは作らない。**32bit Linux カーネルを物理メモリへ置く**のに
//! 要る分だけ: ELF32 / リトルエンディアン / i386 / PT_LOAD の列挙。
//! 動的リンクも再配置もカーネルには無い。

/// ロードすべきセグメント1本ぶん
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// ファイル内のオフセット
    pub offset: usize,
    /// 置き先の物理アドレス
    pub paddr: u32,
    /// ファイルから写すバイト数
    pub filesz: usize,
    /// メモリ上の大きさ。filesz を超えるぶんは **BSS = ゼロで埋める**。
    /// 解凍ステブもこれをやっている — やらないと .bss にゴミが残り、
    /// カーネルが「ゼロのはず」の変数をゴミのまま読む
    pub memsz: usize,
}

/// vmlinux の地図: どこに何を置き、どこから走らせるか
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vmlinux {
    pub segments: Vec<Segment>,
    /// エントリの**物理**アドレス (startup_32)。
    /// e_entry が仮想 (0xC0000000以上) ならセグメントの vaddr→paddr 対応で
    /// 物理へ引き直す。Alpine の vmlinux は最初から物理で書いてある
    pub entry: u32,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// ELF かどうかの見分け (起動経路の振り分けに使う)
pub fn is_elf(data: &[u8]) -> bool {
    data.len() >= 4 && &data[..4] == b"\x7fELF"
}

/// vmlinux (ELF32, i386) を解析する。
///
/// **黙って0を返さない。** 何が期待と違うかをメッセージで断る
/// (このリポジトリの「静かに壊れない」方針)
pub fn parse_vmlinux(data: &[u8]) -> Result<Vmlinux, String> {
    if !is_elf(data) {
        return Err("ELF マジックが無い (vmlinux ではない)".into());
    }
    if data.len() < 52 {
        return Err(format!("ELF が短すぎる ({} バイト)", data.len()));
    }
    if data[4] != 1 {
        return Err("ELF64 が来た (32bit カーネルは ELF32 のはず)".into());
    }
    if data[5] != 1 {
        return Err("ビッグエンディアンの ELF (x86 ではない)".into());
    }
    let machine = u16le(data, 18);
    if machine != 3 {
        return Err(format!("e_machine = {machine} (i386 = 3 ではない)"));
    }

    let e_entry = u32le(data, 24);
    let e_phoff = u32le(data, 28) as usize;
    let e_phentsize = u16le(data, 42) as usize;
    let e_phnum = u16le(data, 44) as usize;

    const PT_LOAD: u32 = 1;
    let mut segments = Vec::new();
    for i in 0..e_phnum {
        let o = e_phoff + i * e_phentsize;
        if o + 32 > data.len() {
            return Err(format!("プログラムヘッダ {i} がファイルの外にある"));
        }
        if u32le(data, o) != PT_LOAD {
            continue;
        }
        let offset = u32le(data, o + 4) as usize;
        let paddr = u32le(data, o + 12);
        let filesz = u32le(data, o + 16) as usize;
        let memsz = u32le(data, o + 20) as usize;
        if offset + filesz > data.len() {
            return Err(format!(
                "セグメント {i} の中身がファイルの外にある (off={offset} filesz={filesz})"
            ));
        }
        segments.push(Segment {
            offset,
            paddr,
            filesz,
            memsz,
        });
    }
    if segments.is_empty() {
        return Err("PT_LOAD が1本も無い".into());
    }

    // エントリを物理へ。仮想アドレスで書かれていれば vaddr→paddr の差で引き直す
    let entry = if e_entry >= 0xC000_0000 {
        let mut found = None;
        for i in 0..e_phnum {
            let o = e_phoff + i * e_phentsize;
            if u32le(data, o) != PT_LOAD {
                continue;
            }
            let vaddr = u32le(data, o + 8);
            let paddr = u32le(data, o + 12);
            let memsz = u32le(data, o + 20);
            if e_entry >= vaddr && e_entry < vaddr.wrapping_add(memsz) {
                found = Some(e_entry - vaddr + paddr);
                break;
            }
        }
        found.ok_or_else(|| format!("エントリ 0x{e_entry:08x} がどのセグメントにも属さない"))?
    } else {
        e_entry
    };

    Ok(Vmlinux { segments, entry })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小のELF32を手で組む (1セグメント)
    fn tiny_elf(e_entry: u32, vaddr: u32, paddr: u32) -> Vec<u8> {
        let mut d = vec![0u8; 52 + 32 + 16];
        d[..4].copy_from_slice(b"\x7fELF");
        d[4] = 1; // ELF32
        d[5] = 1; // LE
        d[18] = 3; // i386
        d[24..28].copy_from_slice(&e_entry.to_le_bytes());
        d[28..32].copy_from_slice(&52u32.to_le_bytes()); // phoff
        d[42..44].copy_from_slice(&32u16.to_le_bytes()); // phentsize
        d[44..46].copy_from_slice(&1u16.to_le_bytes()); // phnum
        let o = 52;
        d[o..o + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        d[o + 4..o + 8].copy_from_slice(&84u32.to_le_bytes()); // offset
        d[o + 8..o + 12].copy_from_slice(&vaddr.to_le_bytes());
        d[o + 12..o + 16].copy_from_slice(&paddr.to_le_bytes());
        d[o + 16..o + 20].copy_from_slice(&8u32.to_le_bytes()); // filesz
        d[o + 20..o + 24].copy_from_slice(&16u32.to_le_bytes()); // memsz
        d
    }

    #[test]
    fn entry_is_translated_from_virtual() {
        let elf = tiny_elf(0xC100_0004, 0xC100_0000, 0x0100_0000);
        let v = parse_vmlinux(&elf).unwrap();
        assert_eq!(v.entry, 0x0100_0004);
        assert_eq!(v.segments.len(), 1);
        assert_eq!(v.segments[0].memsz, 16);
    }

    #[test]
    fn physical_entry_is_kept() {
        let elf = tiny_elf(0x0100_0004, 0xC100_0000, 0x0100_0000);
        assert_eq!(parse_vmlinux(&elf).unwrap().entry, 0x0100_0004);
    }

    #[test]
    fn refuses_non_elf() {
        assert!(parse_vmlinux(b"MZ..").is_err());
    }
}
