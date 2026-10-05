//! 导出物的信封加密（design-security-permission.md §6 的 P1，等 `age` 点头的那一格）。
//!
//! 用 age 的口令模式：口令 → scrypt → 一个一次性 X25519 收件人，输出 armored 文本。
//! 选它而不是在外面自研一层 argon2id 私有头，是因为设计文档选 age 的理由本身就是
//! "CLI 生态能交叉验证我们的实现"——套一层私有头会让 age 命令行读不了我们写的文件，
//! 那条理由就没了。识别一份备份加没加密靠 age 自己的装甲头，不需要第二个标记。
//!
//! 两条不许破的规矩：解密失败就是失败，**不退回读明文**；没给口令就明确说"要口令"，
//! 不含糊成一个通用错误。

use age::scrypt;
use age::secrecy::SecretString;

const ARMOR_HEADER: &str = "-----BEGIN AGE ENCRYPTED FILE-----";

/// 是不是 age 装甲文本。导入靠它决定要不要问口令，所以它必须只看头、不猜内容
pub fn is_armored(text: &str) -> bool {
    text.trim_start().starts_with(ARMOR_HEADER)
}

/// 口令加密成 armored 文本。同一个口令每次产出的字节都不同（盐是随机的），
/// 所以"两次导出字节一样"不是加密生效的判据，往返才是
pub fn encrypt(plain: &[u8], passphrase: &str) -> Result<String, String> {
    let recipient = scrypt::Recipient::new(SecretString::from(passphrase.to_string()));
    age::encrypt_and_armor(&recipient, plain).map_err(|e| format!("加密没起起来：{e}"))
}

/// 口令解密。错口令、坏文件、别人家用钥匙加密的文件，三种要说清是哪一种
pub fn decrypt(armored: &str, passphrase: &str) -> Result<Vec<u8>, String> {
    let identity = scrypt::Identity::new(SecretString::from(passphrase.to_string()));
    age::decrypt(&identity, armored.as_bytes()).map_err(|e| match e {
        age::DecryptError::DecryptionFailed | age::DecryptError::KeyDecryptionFailed => {
            "口令不对：这份备份解不开。不会退回读明文。".to_string()
        }
        // 口令文件配口令身份却"没有匹配的收件人"：那它压根不是口令加密的
        age::DecryptError::NoMatchingKeys => {
            "这份备份不是口令加密的（它要的是身份文件）：aglab 的导出只写口令这一种。"
                .to_string()
        }
        age::DecryptError::Io(_) => "口令不对：这份备份解不开。不会退回读明文。".to_string(),
        other => format!("这份备份解不开：{other}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 往返是"加密真的生效"的唯一判据：装甲文本每次都不一样，比字节没意义
    #[test]
    fn a_passphrase_round_trips_its_own_payload() {
        let plain = b"{\"version\":3,\"records\":[]}";
        let armored = encrypt(plain, "correct horse").expect("加密该成");
        assert!(is_armored(&armored), "产物要认得出是装甲文本");
        assert_ne!(armored.as_bytes(), plain, "加密了就不该还是原文");
        assert_eq!(
            decrypt(&armored, "correct horse").expect("同口令该解得开"),
            plain.to_vec()
        );
    }

    /// 错口令必须是一句说得出口的拒绝，而不是一个通用错误，更不是明文
    #[test]
    fn a_wrong_passphrase_is_refused_out_loud() {
        let armored = encrypt(b"secret payload", "right one").expect("加密该成");
        let error = decrypt(&armored, "wrong one").expect_err("错口令不该解得开");
        assert!(error.contains("口令不对"), "拒绝的理由要写在脸上：{error}");
        assert!(!error.contains("secret payload"), "错误信息里不许漏出明文");
    }

    /// 没加密的文本原样进出：关掉加密时导出字节与今天一致，这条由调用方钉住，
    /// 这里钉的是"识别"这一半——把明文误判成装甲文本，导入就会莫名要口令
    #[test]
    fn plain_text_is_not_mistaken_for_armor() {
        assert!(!is_armored("{\"version\":3}"));
        assert!(is_armored("  \n-----BEGIN AGE ENCRYPTED FILE-----\nxxx"));
    }
}
