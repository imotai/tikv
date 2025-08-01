// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use super::*;

trait GbkCollator: 'static + Send + Sync + std::fmt::Debug {
    const IS_CASE_INSENSITIVE: bool;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK];
}

impl<T: GbkCollator> Collator for T {
    type Charset = CharsetGbk;
    type Weight = u16;

    const IS_CASE_INSENSITIVE: bool = T::IS_CASE_INSENSITIVE;

    #[inline]
    fn char_weight(ch: char) -> Self::Weight {
        // All GBK code point are in BMP, if the incoming character is not, convert it
        // to '?'. This should not happened.
        let r = ch as usize;
        if r > 0xFFFF {
            return '?' as u16;
        }

        (&Self::WEIGHT_TABLE[r * 2..r * 2 + 2]).read_u16().unwrap()
    }

    #[inline]
    fn write_sort_key<W: BufferWriter>(writer: &mut W, bstr: &[u8]) -> Result<usize> {
        let mut bstr_rest = trim_end_padding(bstr);
        let mut n = 0;
        while !bstr_rest.is_empty() {
            match next_utf8_char(bstr_rest) {
                Some((ch, b_next)) => {
                    let weight = Self::char_weight(ch);
                    if weight > 0xFF {
                        writer.write_u16_be(weight)?;
                        n += 2;
                    } else {
                        writer.write_u8(weight as u8)?;
                        n += 1;
                    }
                    bstr_rest = b_next
                }
                _ => {
                    if Self::NEED_TRUNCATE_INVALID_UTF8_RUNE {
                        break;
                    }
                    writer.write_u8(b'?')?;
                    n += 1;
                    bstr_rest = &bstr_rest[1..]
                }
            }
        }
        Ok(n * std::mem::size_of::<u8>())
    }

    #[inline]
    fn sort_compare(a: &[u8], b: &[u8], force_no_pad: bool) -> Result<Ordering> {
        let sa = if force_no_pad { a } else { trim_end_padding(a) };
        let sb = if force_no_pad { b } else { trim_end_padding(b) };
        let mut a_rest = sa;
        let mut b_rest = sb;

        while !a_rest.is_empty() && !b_rest.is_empty() {
            let (ch_a, a_next) = match next_utf8_char(a_rest) {
                Some((ch, next)) => (ch, next),
                None => {
                    if Self::NEED_TRUNCATE_INVALID_UTF8_RUNE {
                        return Ok(Ordering::Equal);
                    } else {
                        ('?', &a_rest[1..])
                    }
                }
            };

            let (ch_b, b_next) = match next_utf8_char(b_rest) {
                Some((ch, next)) => (ch, next),
                None => {
                    if Self::NEED_TRUNCATE_INVALID_UTF8_RUNE {
                        return Ok(Ordering::Equal);
                    } else {
                        ('?', &b_rest[1..])
                    }
                }
            };

            let ord = Self::char_weight(ch_a).cmp(&Self::char_weight(ch_b));
            if ord != Ordering::Equal {
                return Ok(ord);
            }

            a_rest = a_next;
            b_rest = b_next;
        }

        Ok(a_rest.len().cmp(&b_rest.len()))
    }

    #[inline]
    fn sort_hash<H: Hasher>(state: &mut H, bstr: &[u8]) -> Result<()> {
        let mut bstr_rest = trim_end_padding(bstr);
        while !bstr_rest.is_empty() {
            match next_utf8_char(bstr_rest) {
                Some((ch_b, b_next)) => {
                    Self::char_weight(ch_b).hash(state);
                    bstr_rest = b_next
                }
                _ => {
                    if Self::NEED_TRUNCATE_INVALID_UTF8_RUNE {
                        break;
                    }
                    Self::char_weight('?').hash(state);
                    bstr_rest = &bstr_rest[1..]
                }
            }
        }
        Ok(())
    }
}

/// Collator for `gbk_bin` collation with padding behavior (trims right spaces).
#[derive(Debug)]
pub struct CollatorGbkBin;

impl GbkCollator for CollatorGbkBin {
    const IS_CASE_INSENSITIVE: bool = false;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool = false;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK] = GBK_BIN_TABLE;
}

/// Collator for `gbk_chinese_ci` collation with padding behavior (trims right
/// spaces).
#[derive(Debug)]
pub struct CollatorGbkChineseCi;

impl GbkCollator for CollatorGbkChineseCi {
    const IS_CASE_INSENSITIVE: bool = true;
    const NEED_TRUNCATE_INVALID_UTF8_RUNE: bool = true;
    const WEIGHT_TABLE: &'static [u8; TABLE_SIZE_FOR_GBK] = GBK_CHINESE_CI_TABLE;
}

const TABLE_SIZE_FOR_GBK: usize = (0xffff + 1) * 2;

// GBK_BIN_TABLE are the encoding tables from Unicode to GBK code, it is totally
// the same with golang's GBK encoding. If there is no mapping code in GBK, use
// 0x3F(?) instead. It should not happened.
const GBK_BIN_TABLE: &[u8; TABLE_SIZE_FOR_GBK] = include_bytes!("gbk_bin.data");

// GBK_CHINESE_CI_TABLE are the sort key tables for GBK codepoint.
// If there is no mapping code in GBK, use 0x3F(?) instead. It should not
// happened.
const GBK_CHINESE_CI_TABLE: &[u8; TABLE_SIZE_FOR_GBK] = include_bytes!("gbk_chinese_ci.data");
