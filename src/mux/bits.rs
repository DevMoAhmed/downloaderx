//! Bit reading for codec headers (H.264 / H.265 parameter sets).

pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    pub fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.pos += n;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    /// Unsigned Exp-Golomb.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Signed Exp-Golomb.
    pub fn se(&mut self) -> Option<i32> {
        let v = self.ue()?;
        Some(if v % 2 == 1 { v.div_ceil(2) as i32 } else { -((v / 2) as i32) })
    }
}

/// Remove emulation-prevention bytes (00 00 03 -> 00 00) from a NAL unit.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 -> 0, 1, 2, 3
        let mut b = Bits::new(&[0b1010_0110, 0b0100_0000]);
        assert_eq!([b.ue(), b.ue(), b.ue(), b.ue()], [Some(0), Some(1), Some(2), Some(3)]);
        let mut b = Bits::new(&[0b0100_1100]);
        assert_eq!([b.se(), b.se()], [Some(1), Some(-1)]);
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(unescape(&[1, 0, 0, 3, 1, 0, 0, 3]), [1, 0, 0, 1, 0, 0]);
    }
}
