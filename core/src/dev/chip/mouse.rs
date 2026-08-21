//! PS/2 マウス — 8042 の第2ポート (AUX) にぶら下がる素子。
//!
//! キーボードと同じ配線 (クロック+データの2線) で、同じ 8042 の向こう側に
//! 居る。違いは**話す中身**だけ: キーボードは「どのキーが上下したか」を、
//! マウスは「どれだけ動いてどのボタンが押されているか」を、3バイトの
//! パケットで送る。
//!
//! ## 3バイトパケット
//!
//! ```text
//! byte0: | Yov | Xov | Ysign | Xsign | 1 | Mid | Right | Left |
//! byte1: X の移動量 (下位8bit。符号は byte0 の Xsign)
//! byte2: Y の移動量 (同上)。**上が正** — 画面座標 (下が正) と逆なので注意
//! ```
//!
//! bit3 が常に1なのは同期用。OSはこれでパケットの頭を見つけ直す。
//!
//! ## コマンド
//!
//! ホストが 8042 の 0xD4 経由で1バイト送ると、マウスは ACK (0xFA) で答え、
//! コマンドによっては続けてデータを返す。引数を取るコマンド (0xF3 サンプル
//! レート、0xE8 解像度) は次の1バイトを引数として受け、それにも ACK を返す。
//! リセット (0xFF) だけは ACK の後に自己診断の結果 (0xAA) と ID (0x00) が続く。
//!
//! ここで作るのは**素の3ボタンPS/2マウス (ID 0)**。ホイール付き (IntelliMouse、
//! ID 3) はサンプルレートを 200→100→80 と設定する「合言葉」で名乗るが、
//! 名乗らなければ OS は素のマウスとして扱うので、要るまで作らない。

/// 1パケットの移動量の上限 (9bit 符号付き。超えたら overflow ビット)
const MAX_DELTA: i32 = 255;

#[derive(Debug)]
pub struct Mouse {
    /// ストリームモードで報告中か (0xF4 で入り、0xF5/リセットで出る)
    pub reporting: bool,
    sample_rate: u8,
    resolution: u8,
    scaling2: bool,
    /// 引数を待っているコマンド (0xF3 / 0xE8)
    pending: Option<u8>,
    /// 今押されているボタン (bit0=左 bit1=右 bit2=中)
    buttons: u8,
    /// ホストへ返すバイト列 (8042 が AUX として取り出す)
    out: std::collections::VecDeque<u8>,
}

impl Default for Mouse {
    fn default() -> Self {
        Self::new()
    }
}

impl Mouse {
    pub fn new() -> Self {
        Self {
            reporting: false,
            sample_rate: 100,
            resolution: 2,
            scaling2: false,
            pending: None,
            buttons: 0,
            out: std::collections::VecDeque::new(),
        }
    }

    /// ホストからの1バイト (8042 の 0xD4 の次のバイト)
    pub fn command(&mut self, b: u8) {
        if let Some(cmd) = self.pending.take() {
            match cmd {
                0xF3 => self.sample_rate = b,
                0xE8 => self.resolution = b,
                _ => {}
            }
            self.out.push_back(0xFA);
            return;
        }
        match b {
            // リセット: ACK → 自己診断OK → ID。既定値に戻り、報告は止まる
            0xFF => {
                *self = Self::new();
                self.out.extend([0xFA, 0xAA, 0x00]);
            }
            0xF6 => {
                // 既定値 (レート100・解像度4カウント/mm・スケーリング1:1・報告停止)
                self.sample_rate = 100;
                self.resolution = 2;
                self.scaling2 = false;
                self.reporting = false;
                self.out.push_back(0xFA);
            }
            0xF2 => self.out.extend([0xFA, 0x00]), // ID: 素のPS/2マウス
            0xF4 => {
                self.reporting = true;
                self.out.push_back(0xFA);
            }
            0xF5 => {
                self.reporting = false;
                self.out.push_back(0xFA);
            }
            0xF3 | 0xE8 => {
                self.pending = Some(b);
                self.out.push_back(0xFA);
            }
            0xE6 => {
                self.scaling2 = false;
                self.out.push_back(0xFA);
            }
            0xE7 => {
                self.scaling2 = true;
                self.out.push_back(0xFA);
            }
            // 状態の問い合わせ: ACK → (モード/ボタン, 解像度, レート)
            0xE9 => {
                let st = (self.reporting as u8) << 5 | (self.scaling2 as u8) << 4 | self.buttons;
                self.out
                    .extend([0xFA, st, self.resolution, self.sample_rate]);
            }
            // ストリーム/リモート/ラップの各モードと、ラップ解除。受けるだけ
            0xEA | 0xF0 | 0xEE | 0xEC => self.out.push_back(0xFA),
            // リモートモードの読み出し: ACK → 動き無しのパケット
            0xEB => {
                self.out.push_back(0xFA);
                self.push_packet(0, 0);
            }
            // 知らないコマンドは RESEND (0xFE)。実機もそう答える
            _ => self.out.push_back(0xFE),
        }
    }

    /// ホスト側の動き: `dx` は右が正、`dy` は**下が正** (画面座標)。
    /// `buttons` は bit0=左 bit1=右 bit2=中。報告中でなければ捨てる
    /// (実機も報告停止中は何も送らない)
    pub fn motion(&mut self, dx: i32, dy: i32, buttons: u8) {
        self.buttons = buttons & 7;
        if !self.reporting {
            return;
        }
        // PS/2 の Y は上が正
        self.push_packet(dx, -dy);
    }

    fn push_packet(&mut self, dx: i32, dy: i32) {
        let xov = dx.abs() > MAX_DELTA;
        let yov = dy.abs() > MAX_DELTA;
        let dx = dx.clamp(-MAX_DELTA, MAX_DELTA);
        let dy = dy.clamp(-MAX_DELTA, MAX_DELTA);
        let b0 = 0x08
            | (yov as u8) << 7
            | (xov as u8) << 6
            | ((dy < 0) as u8) << 5
            | ((dx < 0) as u8) << 4
            | self.buttons;
        self.out.extend([b0, dx as u8, dy as u8]);
    }

    /// 8042 が取り出す次のバイト
    pub fn pop(&mut self) -> Option<u8> {
        self.out.pop_front()
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }
}

impl Mouse {
    pub fn save(&self, w: &mut crate::snapshot::Writer) {
        w.bool(self.reporting);
        w.u8(self.sample_rate);
        w.u8(self.resolution);
        w.bool(self.scaling2);
        w.opt_u8(self.pending);
        w.u8(self.buttons);
        let out: Vec<u8> = self.out.iter().copied().collect();
        w.bytes(&out);
    }

    pub fn load(&mut self, r: &mut crate::snapshot::Reader) -> Result<(), String> {
        self.reporting = r.bool()?;
        self.sample_rate = r.u8()?;
        self.resolution = r.u8()?;
        self.scaling2 = r.bool()?;
        self.pending = r.opt_u8()?;
        self.buttons = r.u8()?;
        self.out = r.bytes()?.into();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(m: &mut Mouse) -> Vec<u8> {
        let mut v = vec![];
        while let Some(b) = m.pop() {
            v.push(b);
        }
        v
    }

    /// リセットは ACK → BAT OK → ID の3バイト。以後は報告停止
    #[test]
    fn reset_answers_ack_bat_id() {
        let mut m = Mouse::new();
        m.reporting = true;
        m.command(0xFF);
        assert_eq!(drain(&mut m), [0xFA, 0xAA, 0x00]);
        assert!(!m.reporting);
    }

    /// 引数付きコマンドは、引数にも ACK
    #[test]
    fn sample_rate_takes_a_parameter() {
        let mut m = Mouse::new();
        m.command(0xF3);
        m.command(200);
        assert_eq!(drain(&mut m), [0xFA, 0xFA]);
        assert_eq!(m.sample_rate, 200);
    }

    /// 報告中に動かすと3バイト。Yは上が正なので、画面の下向きは符号が立つ
    #[test]
    fn motion_makes_a_packet_with_ps2_sign_convention() {
        let mut m = Mouse::new();
        m.motion(5, 3, 1); // 報告前は捨てる
        assert!(drain(&mut m).is_empty());
        m.command(0xF4);
        drain(&mut m);
        m.motion(5, 3, 0b001); // 右へ5、下へ3、左ボタン
        let p = drain(&mut m);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0] & 0x08, 0x08, "同期ビット");
        assert_eq!(p[0] & 0x07, 0b001, "左ボタン");
        assert_eq!(p[0] & 0x10, 0, "Xは正");
        assert_eq!(p[0] & 0x20, 0x20, "画面の下向き = PS/2では負");
        assert_eq!(p[1], 5);
        assert_eq!(p[2], (-3i8) as u8);
    }

    /// 大きすぎる動きは overflow を立てて飽和する
    #[test]
    fn huge_motion_sets_overflow() {
        let mut m = Mouse::new();
        m.command(0xF4);
        drain(&mut m);
        m.motion(1000, 0, 0);
        let p = drain(&mut m);
        assert_eq!(p[0] & 0x40, 0x40, "X overflow");
        assert_eq!(p[1], 255);
    }
}
