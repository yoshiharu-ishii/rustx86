//! 機械の状態をまるごと保存し、あとから戻す。
//!
//! 「割り込みが入る直前で止めて保存し、あとで何度でもそこから始める」ができる。
//! 実機では不可能な、エミュレータならではの道具である。
//!
//! ## 形式
//!
//! 素朴なバイナリ列で書く。JSONにしないのは**メモリが大きいから**で、
//! 1MBのバイト配列をJSONの数値配列にすると数MBに膨れる。
//! JSONで束ねるのは呼び出し側 (ブラウザ) の仕事にして、ここは中身だけを返す。
//!
//! メモリとディスクは**連長圧縮 (RLE)** をかける。エミュレートしている
//! メモリはほとんどがゼロなので、1MBが数KBまで縮む。
//!
//! ## 何を保存するか
//!
//! CPUだけでは足りない。**装置の状態も一緒でなければ再開できない**。
//! PICのマスクが失われれば以後の割り込みが来なくなり、PITのカウンタが
//! 戻れば時計が飛ぶ。「状態」とはCPUと装置とメモリの全部である。

const MAGIC: &[u8; 8] = b"RX86SNAP";
/// 形式の版。合わないものは読まない (黙って壊れた状態で動き出さないため)
pub const VERSION: u16 = 1;

/// 順番に書いていくだけの器
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// 長さを付けて生バイト列を書く
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }
    /// 長さを付けて連長圧縮して書く
    pub fn rle(&mut self, v: &[u8]) {
        let packed = rle_encode(v);
        self.u32(v.len() as u32);
        self.bytes(&packed);
    }
    pub fn opt_u8(&mut self, v: Option<u8>) {
        match v {
            Some(x) => {
                self.bool(true);
                self.u8(x);
            }
            None => self.bool(false),
        }
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// 順番に読んでいくだけの器。足りなければエラーにする
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or("スナップショットが壊れている")?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or("スナップショットが途中で終わっている")?;
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, String> {
        Ok(self.u8()? != 0)
    }
    pub fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn rle(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        let packed = self.bytes()?;
        let out = rle_decode(&packed, len)?;
        Ok(out)
    }
    pub fn opt_u8(&mut self) -> Result<Option<u8>, String> {
        Ok(if self.bool()? { Some(self.u8()?) } else { None })
    }
}

/// 連長圧縮。`[値, 回数]` の並びにする。
///
/// 回数は1バイトなので最大255。凝った方式にしないのは、**狙いがゼロの海を
/// 潰すこと**だけだからである。実際1MBのメモリが数KBになる。
fn rle_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let v = data[i];
        let mut n = 1usize;
        while i + n < data.len() && data[i + n] == v && n < 255 {
            n += 1;
        }
        out.push(v);
        out.push(n as u8);
        i += n;
    }
    out
}

fn rle_decode(packed: &[u8], expect: usize) -> Result<Vec<u8>, String> {
    if packed.len() % 2 != 0 {
        return Err("圧縮データの長さが奇数".into());
    }
    let mut out = Vec::with_capacity(expect);
    for pair in packed.chunks_exact(2) {
        out.extend(std::iter::repeat(pair[0]).take(pair[1] as usize));
    }
    if out.len() != expect {
        return Err(format!(
            "展開後の大きさが合わない ({} != {expect})",
            out.len()
        ));
    }
    Ok(out)
}

/// 先頭の印と版を書く
pub fn write_header(w: &mut Writer) {
    w.buf.extend_from_slice(MAGIC);
    w.u16(VERSION);
}

/// 先頭の印と版を確かめる
pub fn read_header(r: &mut Reader) -> Result<(), String> {
    let magic = r.take(8)?;
    if magic != MAGIC {
        return Err("スナップショットではない".into());
    }
    let v = r.u16()?;
    if v != VERSION {
        return Err(format!("形式の版が違う (保存 {v} / 対応 {VERSION})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_round_trips() {
        for case in [
            vec![],
            vec![0u8; 1000],
            vec![1, 1, 2, 3, 3, 3],
            (0..=255u8).collect::<Vec<_>>(),
            vec![7u8; 300], // 255を跨ぐ
        ] {
            let packed = rle_encode(&case);
            assert_eq!(rle_decode(&packed, case.len()).unwrap(), case);
        }
    }

    /// ゼロの海が実際に縮むこと (これが目的)
    #[test]
    fn zeros_compress_hard() {
        let packed = rle_encode(&vec![0u8; 1 << 20]);
        assert!(packed.len() < 10_000, "1MBのゼロが {} バイト", packed.len());
    }

    #[test]
    fn reader_rejects_truncated_data() {
        let mut w = Writer::new();
        write_header(&mut w);
        w.u32(0xDEAD_BEEF);
        let mut r = Reader::new(&w.buf[..w.buf.len() - 2]);
        read_header(&mut r).unwrap();
        assert!(r.u32().is_err(), "途中で切れたデータを受け入れてはいけない");
    }
}
