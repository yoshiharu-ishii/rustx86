//! JITヘルパ — 生成コードから呼ばれる**記録しない**メモリアクセス
//! (F1d、ADR-0008の脱出モデル)。
//!
//! 従来経路との違いは1点だけ: フォールトしそうなとき `note_fault` せず
//! None/false を返す。生成コードはこれを合図に**状態を1つも変えずに脱出**し、
//! インタプリタが同じ命令をやり直して正式に裁く (#PFの記録・配送は従来経路)。
//! 意味論は fast系 ([`super::fast`]) とALUヘルパへ委譲し、二重実装しない。

use crate::Machine;

impl Machine {
    /// JIT用の32bit読み (F1d-b)。
    ///
    /// 脱出は保守的でよい (余計に脱出しても、やり直しで同じ結果になる) ので、
    /// ページ跨ぎは無条件で None に倒す。Some の道は read32 の速い道と同じ部品
    /// (translate_for + read_phys32) — 意味論を二重実装しない
    pub fn jit_try_read32(&self, addr: u32) -> Option<u32> {
        if addr & 0xFFF > 0xFFC {
            return None; // ページ跨ぎ (稀) はインタプリタに任せる
        }
        match self.translate_for(addr, false) {
            Ok(pa) => Some(self.read_phys32(pa)),
            Err(_) => None,
        }
    }

    /// JIT用の8bit読み。1バイトはページを跨げないので跨ぎ検査なし
    pub fn jit_try_read8(&self, addr: u32) -> Option<u8> {
        match self.translate_for(addr, false) {
            Ok(pa) => Some(self.read_phys8(pa)),
            Err(_) => None,
        }
    }

    /// JIT用の16bit読み。跨ぎは脱出 (インタプリタに任せる)
    pub fn jit_try_read16(&self, addr: u32) -> Option<u16> {
        if addr & 0xFFF > 0xFFE {
            return None;
        }
        match self.translate_for(addr, false) {
            Ok(pa) => Some(u16::from_le_bytes([
                self.read_phys8(pa),
                self.read_phys8(pa.wrapping_add(1)),
            ])),
            Err(_) => None,
        }
    }

    /// JIT用の32bitストア (F1d-c)。**意味論はfast_write32へ委譲** —
    /// note_write (自己書き換え検出、ADR-0020) も VRAM窓の脱出も dbg も
    /// そちらの1本に畳まれている。返り値: true=完了 / false=脱出 (何も書いてない。
    /// インタプリタが同じ命令をやり直す)。
    /// **書いた後の自ページ世代照合は生成コード側の責務** (jit.rsのn+1契約)
    pub fn jit_try_write32(&mut self, seg: usize, off: u32, val: u32) -> bool {
        self.fast_write32(seg, off, val).is_some()
    }

    /// JIT用の8bitストア (F1d-f)。意味論はfast_write8へ委譲 — 32bit版と同じ契約
    pub fn jit_try_write8(&mut self, seg: usize, off: u32, val: u8) -> bool {
        self.fast_write8(seg, off, val).is_some()
    }

    /// JIT用の8bit RMW (`alu [m8], b`)。32bit版と同じ形 — ALUは従来と同じ alu8
    pub fn jit_try_rmw8(&mut self, seg: usize, off: u32, kind: u8, b: u8) -> bool {
        let Some(pa) = self.fast_rmw8_addr(seg, off) else {
            return false;
        };
        let a = self.mem[pa];
        let v = crate::cpu::alu::alu8(&mut self.cpu, kind, a, b);
        self.mem[pa] = v;
        self.dcache.note_write(pa as u32);
        true
    }

    /// JIT用の inc/dec [mem] (Grp5/0,1 — CF不変)。意味論は従来と同じ inc_dec_w
    pub fn jit_try_incdec32(&mut self, seg: usize, off: u32, dec: bool) -> bool {
        let Some(pa) = self.fast_rmw32_addr(seg, off) else {
            return false;
        };
        let a = u32::from_le_bytes(self.mem[pa..pa + 4].try_into().unwrap());
        let v = crate::cpu::alu::inc_dec_w(&mut self.cpu, a, dec, true);
        self.mem[pa..pa + 4].copy_from_slice(&v.to_le_bytes());
        self.dcache.note_write(pa as u32);
        true
    }

    /// JIT用のRMW (`alu [mem], b`)。exec.rsのfast RMW armの写し —
    /// 書き込み権限で先に変換 (writable⊆readable) するので、cc更新後に
    /// 失敗する道が無い。ALUは従来と同じ alu_w、書いたら note_write
    pub fn jit_try_rmw32(&mut self, seg: usize, off: u32, kind: u8, b: u32) -> bool {
        let Some(pa) = self.fast_rmw32_addr(seg, off) else {
            return false;
        };
        let a = u32::from_le_bytes(self.mem[pa..pa + 4].try_into().unwrap());
        let v = crate::cpu::alu::alu_w(&mut self.cpu, kind, a, b, true);
        self.mem[pa..pa + 4].copy_from_slice(&v.to_le_bytes());
        self.dcache.note_write(pa as u32);
        true
    }

    /// JIT用のpush (dcache/exec.rsのfast_push32と同一実装 — あちらが委譲してくる)。
    /// SSが平坦・32bitスタックで書き込みが確定するときだけSPを動かしてtrue
    pub fn jit_try_push32(&mut self, v: u32) -> bool {
        if !self.cpu.hidden[crate::cpu::SS].big {
            return false;
        }
        let sp = self.cpu.regs[crate::cpu::SP].wrapping_sub(4);
        if self.fast_write32(crate::cpu::SS, sp, v).is_none() {
            return false;
        }
        self.cpu.regs[crate::cpu::SP] = sp;
        true
    }

    /// JIT用のleave (SP←BP、BP←pop — exec.rsのLeave速い道と同一順序)。
    /// 読みが確定してから両レジスタを動かす。false = 脱出 (SP/BP無傷)
    pub fn jit_try_leave(&mut self) -> bool {
        if !self.cpu.hidden[crate::cpu::SS].big {
            return false;
        }
        let bp = self.cpu.regs[crate::cpu::BP];
        let Some(v) = self.fast_read32(crate::cpu::SS, bp) else {
            return false;
        };
        self.cpu.regs[crate::cpu::SP] = bp.wrapping_add(4);
        self.cpu.regs[crate::cpu::BP] = v;
        true
    }

    /// JIT用のpop。読みが確定したときだけSPを確定 (push32と同じ約束)
    pub fn jit_try_pop32(&mut self) -> Option<u32> {
        if !self.cpu.hidden[crate::cpu::SS].big {
            return None;
        }
        let sp = self.cpu.regs[crate::cpu::SP];
        let v = self.fast_read32(crate::cpu::SS, sp)?;
        self.cpu.regs[crate::cpu::SP] = sp.wrapping_add(4);
        Some(v)
    }
}
