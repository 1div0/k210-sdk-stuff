//! AES peripheral
use core::cmp;
use core::convert::TryInto;
use core::sync::atomic::{self, Ordering};
use k210_hal::pac;
use pac::aes::data_in_flag::DATA_IN_FLAG_A;
use pac::aes::data_out_flag::DATA_OUT_FLAG_A;
use pac::aes::en::EN_A;
use pac::aes::finish::FINISH_A;
use pac::aes::endian::ENDIAN_A;
use pac::aes::mode_ctl::KEY_MODE_A;
use pac::aes::tag_chk::TAG_CHK_A;

use crate::soc::sysctl;

pub use pac::aes::mode_ctl::CIPHER_MODE_A as cipher_mode;
pub use pac::aes::encrypt_sel::ENCRYPT_SEL_A as encrypt_sel;

/** Read four bytes from a byte slice as a little-endian u32 (pretend the slice is zero-padded). */
fn read4pad(arr: &[u8], ofs: usize) -> u32 {
    u32::from_le_bytes([
        *arr.get(ofs + 0).unwrap_or(&0),
        *arr.get(ofs + 1).unwrap_or(&0),
        *arr.get(ofs + 2).unwrap_or(&0),
        *arr.get(ofs + 3).unwrap_or(&0),
    ])
}

/** Write up to four bytes to a byte slice, or as many as fit. */
fn write4pad(arr: &mut [u8], ofs: usize, val: u32) {
    let n = cmp::min(arr.len() - ofs, 4);
    arr[ofs..ofs+n].copy_from_slice(&val.to_le_bytes()[0..n]);
}

/** AES operation (encrypt or decrypt) using hardware engine. Takes a &mut
 * AES as only one operation can be active at a time.
 *
 * Supported modes:
 *
 * Mode Keybits           Extra input              Extra output
 * ---- ---------------   ------------------------ ----------------
 * ECB  128 / 192 / 256   no IV
 * CBC  128 / 192 / 256   128 bit IV
 * GCM  128 / 192 / 256   96 bit IV + ? bytes AAD) 128 bit tag (optional)
 */
pub fn run(
        aes: &mut pac::AES,
        cipher_mode: cipher_mode,
        encrypt_sel: encrypt_sel,
        key: &[u8],
        iv: &[u8],
        aad: &[u8],
        ind: &[u8],
        outd: &mut [u8],
        tag: &mut [u8],
    )
{
    match cipher_mode {
        cipher_mode::ECB => assert!(iv.len() == 0 && aad.len() == 0),
        cipher_mode::CBC => assert!(iv.len() == 16 && aad.len() == 0),
        cipher_mode::GCM => assert!(iv.len() == 12),
    }
    let key_mode = match key.len() {
        16 => KEY_MODE_A::AES128,
        24 => KEY_MODE_A::AES192,
        32 => KEY_MODE_A::AES256,
        _ => panic!("invalid key size for AES"),
    };
    unsafe {
        aes.endian.write(|w| w.endian().variant(ENDIAN_A::LE));

        for i in 0..4 { // key is always at least 128 bit wide
            aes.key[i].write(|w| w.bits(
                u32::from_le_bytes(key[key.len() - i*4 - 4..key.len() - i*4].try_into().unwrap())
            ))
        }
        for i in 4..key.len()/4 {
            aes.key_ext[i - 4].write(|w| w.bits(
                u32::from_le_bytes(key[key.len() - i*4 - 4..key.len() - i*4].try_into().unwrap())
            ))
        }
        for i in 0..iv.len()/4 {
            aes.iv[i].write(|w| w.bits(
                u32::from_le_bytes(iv[iv.len() - i*4 - 4..iv.len() - i*4].try_into().unwrap())
            ))
        }
        aes.mode_ctl.write(|w|
            w.cipher_mode().variant(cipher_mode)
             .key_mode().variant(key_mode));
        aes.encrypt_sel.write(|w|
            w.encrypt_sel().variant(encrypt_sel));
        aes.aad_num.write(|w| w.bits((aad.len() as u32).wrapping_sub(1)));
        aes.pc_num.write(|w| w.bits((ind.len() as u32).wrapping_sub(1)));

        // Turn on engine
        aes.en.write(|w|
            w.en().variant(EN_A::ENABLE));

        // Write AAD first
        for i in 0..(aad.len()+3)/4 {
            while aes.data_in_flag.read().data_in_flag() != DATA_IN_FLAG_A::CAN_INPUT {
                atomic::compiler_fence(Ordering::SeqCst)
            }
            aes.aad_data.write(|w| w.bits(read4pad(aad, i * 4)));
        }

        // Send and receive plaintext/ciphertext 
        let mut iptr = 0;
        let mut optr = 0;
        while optr < ind.len() {
            while iptr < ind.len() && aes.data_in_flag.read().data_in_flag() == DATA_IN_FLAG_A::CAN_INPUT {
                aes.text_data.write(|w| w.bits(read4pad(ind, iptr)));
                iptr += 4;
            }
            while aes.data_out_flag.read().data_out_flag() == DATA_OUT_FLAG_A::CAN_OUTPUT {
                write4pad(outd, optr, aes.out_data.read().bits());
                optr += 4;
            }
        }

        if cipher_mode == cipher_mode::GCM && tag.len() != 0 {
            // Read and store tag, if requested
            // TODO: the engine also supports writing a tag through gcm_in_tag
            // and verifying it, presumably in linear time.
            while aes.tag_in_flag.read().tag_in_flag() != DATA_IN_FLAG_A::CAN_INPUT {
                atomic::compiler_fence(Ordering::SeqCst)
            }
            // Write a fake tag
            aes.gcm_in_tag[0].write(|w| w.bits(0));
            aes.gcm_in_tag[1].write(|w| w.bits(0));
            aes.gcm_in_tag[2].write(|w| w.bits(0));
            aes.gcm_in_tag[3].write(|w| w.bits(0));
            // Wait until tag was checked
            while aes.tag_chk.read().tag_chk() == TAG_CHK_A::BUSY {
                atomic::compiler_fence(Ordering::SeqCst)
            }

            for i in 0..4 {
                let val = aes.gcm_out_tag[3 - i].read().bits();
                tag[i*4..i*4+4].copy_from_slice(&val.to_be_bytes());
            }
        }

        // Wait until AES engine finished
        while aes.finish.read().finish() != FINISH_A::FINISHED {
            atomic::compiler_fence(Ordering::SeqCst)
        }
    }
}
