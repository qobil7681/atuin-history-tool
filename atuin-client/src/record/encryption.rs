use atuin_common::record::{AdditonalData, DecryptedData, EncryptedData, Encryption};
use base64::{engine::general_purpose, Engine};
use eyre::{ensure, Context, Result};
use rusty_paserk::{
    id::EncodeId,
    wrap::{LocalWrapperExt, Pie},
};
use rusty_paseto::core::{
    ImplicitAssertion, Key, Local, Paseto, PasetoNonce, PasetoSymmetricKey, Payload, V4,
};
use serde::{Deserialize, Serialize};

/// Use PASETO V4 Local encryption using the additional data as an implicit assertion.
#[allow(non_camel_case_types)]
pub struct PASETO_V4;

/*
Why do we use a random content-encryption key?
Originally I was planning on using a derived key for encryption based on additional data.
This would be a lot more secure than using the master key directly.

However, there's an established norm of using a random key. This scheme might be otherwise known as
- client-side encryption
- envelope encryption
- key wrapping

A HSM (Hardward security module) provider, eg AWS, Azure, GCP, or even a physical device like a yubikey
will have some keys that they keep to themselves. These keys never leave their physical hardware.
If they never leave the hardward, then encrypting large amounts of data means giving them the data and waiting.
This is not a practical solution. Instead, generate a unique key for your data, encrypt that using your HSM
and then store that with your data.

See
 - <https://docs.aws.amazon.com/wellarchitected/latest/financial-services-industry-lens/use-envelope-encryption-with-customer-master-keys.html>
 - <https://cloud.google.com/kms/docs/envelope-encryption>
 - <https://learn.microsoft.com/en-us/azure/storage/blobs/client-side-encryption?tabs=dotnet#encryption-and-decryption-via-the-envelope-technique>
 - <https://www.yubico.com/gb/product/yubihsm-2-fips/>
 - <https://cheatsheetseries.owasp.org/cheatsheets/Cryptographic_Storage_Cheat_Sheet.html#encrypting-stored-keys>

Why would we care? In the past we have recieved some requests for company solutions. If in future we can configure a
KMS service with little effort, then that would solve a lot of issues for their security team.

Even for personal use, if a user is not comfortable with sharing keys between hosts,
GCP HSM costs $1/month and $0.03 per 10,000 key operations. Assuming an active user runs
1000 atuin records a day, that would only cost them $1 and 10 cent a month.

Additionally, key rotations are much simpler using this scheme. Rotating a key is as simple as re-encrypting the CEK, and not the message contents.
This makes it very fast to rotate a key in bulk.

For future reference, with asymmetric encryption, you can encrypt the CEK without the HSM's involvement, but decrypting
will need the HSM. This allows the encryption path to still be extremely fast (no network calls) but downloads/decryption
that happens in the background can make the network calls to the HSM
*/

impl Encryption for PASETO_V4 {
    fn re_encrypt(
        mut data: EncryptedData,
        _ad: AdditonalData,
        old_key: &[u8; 32],
        new_key: &[u8; 32],
    ) -> Result<EncryptedData> {
        let cek = Self::decrypt_cek(data.content_encryption_key, old_key)?;
        data.content_encryption_key = Self::encrypt_cek(cek, new_key);
        Ok(data)
    }

    fn encrypt(data: DecryptedData, ad: AdditonalData, key: &[u8; 32]) -> EncryptedData {
        // generate a random key for this entry
        // aka content-encryption-key (CEK)
        let random_key =
            PasetoSymmetricKey::from(Key::try_new_random().expect("could not source from random"));

        // encode the implicit assertions
        let assertions = Assertions::from(ad).encode();

        // build the payload and encrypt the token
        let payload = general_purpose::URL_SAFE_NO_PAD.encode(data.0);
        let nonce = Key::<32>::try_new_random().expect("could not source from random");
        let nonce = PasetoNonce::<V4, Local>::from(&nonce);

        let token = Paseto::<V4, Local>::builder()
            .set_payload(Payload::from(payload.as_str()))
            .set_implicit_assertion(ImplicitAssertion::from(assertions.as_str()))
            .try_encrypt(&random_key, &nonce)
            .expect("error encrypting atuin data");

        EncryptedData {
            data: token,
            content_encryption_key: Self::encrypt_cek(random_key, key),
        }
    }

    fn decrypt(data: EncryptedData, ad: AdditonalData, key: &[u8; 32]) -> Result<DecryptedData> {
        let token = data.data;
        let cek = Self::decrypt_cek(data.content_encryption_key, key)?;

        // encode the implicit assertions
        let assertions = Assertions::from(ad).encode();

        // decrypt the payload with the footer and implicit assertions
        let payload = Paseto::<V4, Local>::try_decrypt(
            &token,
            &cek,
            None,
            ImplicitAssertion::from(&*assertions),
        )
        .context("could not decrypt entry")?;

        let data = general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
        Ok(DecryptedData(data))
    }
}

impl PASETO_V4 {
    fn decrypt_cek(wrapped_cek: String, key: &[u8; 32]) -> Result<PasetoSymmetricKey<V4, Local>> {
        let wrapping_key = PasetoSymmetricKey::from(Key::from(key));

        let AtuinFooter { kid, wpk } = serde_json::from_str(&wrapped_cek)
            .context("wrapped cek did not contain the correct contents")?;

        // check that the wrapping key matches the required key to decrypt.
        // In future, we could support multiple keys and use this key to
        // look up the key rather than only allow one key.
        // For now though we will only support the one key and key rotation will
        // have to be a hard reset
        let current_kid = wrapping_key.encode_id();
        ensure!(
            current_kid == kid,
            "attempting to decrypt with incorrect key. currently using {current_kid}, expecting {kid}"
        );

        // decrypt the random key
        let mut wrapped_key = wpk.into_bytes();
        Ok(Pie::unwrap_local(&mut wrapped_key, &wrapping_key)?)
    }

    fn encrypt_cek(cek: PasetoSymmetricKey<V4, Local>, key: &[u8; 32]) -> String {
        // aka key-encryption-key (KEK)
        let wrapping_key = PasetoSymmetricKey::from(Key::from(key));

        // wrap the random key so we can decrypt it later
        let key_nonce = Key::<32>::try_new_random().expect("could not source from random");
        let wrapped_cek = AtuinFooter {
            wpk: Pie::wrap_local(&cek, &wrapping_key, &key_nonce),
            kid: wrapping_key.encode_id(),
        };
        serde_json::to_string(&wrapped_cek).expect("could not serialize wrapped cek")
    }
}

#[derive(Serialize, Deserialize)]
/// Well-known footer claims for decrypting. This is not encrypted but is stored in the record.
/// <https://github.com/paseto-standard/paseto-spec/blob/master/docs/02-Implementation-Guide/04-Claims.md#optional-footer-claims>
struct AtuinFooter {
    /// Wrapped key
    wpk: String,
    /// ID of the key which was used to wrap
    kid: String,
}

/// Used in the implicit assertions. This is not encrypted and not stored in the data blob.
// This cannot be changed, otherwise it breaks the authenticated encryption.
#[derive(Debug, Copy, Clone, Serialize)]
struct Assertions<'a> {
    id: &'a str,
    version: &'a str,
    tag: &'a str,
    host: &'a str,
}

impl<'a> From<AdditonalData<'a>> for Assertions<'a> {
    fn from(ad: AdditonalData<'a>) -> Self {
        Self {
            id: ad.id,
            version: ad.version,
            tag: ad.tag,
            host: ad.host,
        }
    }
}

impl Assertions<'_> {
    fn encode(&self) -> String {
        serde_json::to_string(self).expect("could not serialize implicit assertions")
    }
}

#[cfg(test)]
mod tests {
    use atuin_common::record::Record;

    use super::*;

    #[test]
    fn round_trip() {
        let key = Key::try_new_random().unwrap();

        let ad = AdditonalData {
            id: "foo",
            version: "v0",
            tag: "kv",
            host: "1234",
        };

        let data = DecryptedData(vec![1, 2, 3, 4]);

        let encrypted = PASETO_V4::encrypt(data.clone(), ad, &key);
        let decrypted = PASETO_V4::decrypt(encrypted, ad, &key).unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn same_entry_different_output() {
        let key = Key::try_new_random().unwrap();

        let ad = AdditonalData {
            id: "foo",
            version: "v0",
            tag: "kv",
            host: "1234",
        };

        let data = DecryptedData(vec![1, 2, 3, 4]);

        let encrypted = PASETO_V4::encrypt(data.clone(), ad, &key);
        let encrypted2 = PASETO_V4::encrypt(data, ad, &key);

        assert_ne!(
            encrypted.data, encrypted2.data,
            "re-encrypting the same contents should have different output due to key randomization"
        );
    }

    #[test]
    fn cannot_decrypt_different_key() {
        let key = Key::try_new_random().unwrap();
        let fake_key = Key::try_new_random().unwrap();

        let ad = AdditonalData {
            id: "foo",
            version: "v0",
            tag: "kv",
            host: "1234",
        };

        let data = DecryptedData(vec![1, 2, 3, 4]);

        let encrypted = PASETO_V4::encrypt(data, ad, &key);
        let _ = PASETO_V4::decrypt(encrypted, ad, &fake_key).unwrap_err();
    }

    #[test]
    fn cannot_decrypt_different_id() {
        let key = Key::try_new_random().unwrap();

        let ad = AdditonalData {
            id: "foo",
            version: "v0",
            tag: "kv",
            host: "1234",
        };

        let data = DecryptedData(vec![1, 2, 3, 4]);

        let encrypted = PASETO_V4::encrypt(data, ad, &key);

        let ad = AdditonalData {
            id: "foo1",
            version: "v0",
            tag: "kv",
            host: "1234",
        };
        let _ = PASETO_V4::decrypt(encrypted, ad, &key).unwrap_err();
    }

    #[test]
    fn re_encrypt_round_trip() {
        let key1 = Key::try_new_random().unwrap();
        let key2 = Key::try_new_random().unwrap();

        let ad = AdditonalData {
            id: "foo",
            version: "v0",
            tag: "kv",
            host: "1234",
        };

        let data = DecryptedData(vec![1, 2, 3, 4]);

        let encrypted1 = PASETO_V4::encrypt(data.clone(), ad, &key1);
        let encrypted2 = PASETO_V4::re_encrypt(encrypted1.clone(), ad, &key1, &key2).unwrap();

        // we only re-encrypt the content keys
        assert_eq!(encrypted1.data, encrypted2.data);
        assert_ne!(encrypted1.content_encryption_key, encrypted2.content_encryption_key);

        let decrypted = PASETO_V4::decrypt(encrypted2, ad, &key2).unwrap();

        assert_eq!(decrypted, data);
    }

    #[test]
    fn full_record_round_trip() {
        let key = [0x55; 32];
        let record = Record::builder()
            .id("1".to_owned())
            .version("v0".to_owned())
            .tag("kv".to_owned())
            .host("host1".to_owned())
            .timestamp(1687244806000000)
            .data(DecryptedData(vec![1, 2, 3, 4]))
            .build();

        let encrypted = record.encrypt::<PASETO_V4>(&key);

        assert!(!encrypted.data.data.is_empty());
        assert!(!encrypted.data.content_encryption_key.is_empty());
        assert_eq!(encrypted.id, "1");
        assert_eq!(encrypted.host, "host1");
        assert_eq!(encrypted.version, "v0");
        assert_eq!(encrypted.tag, "kv");
        assert_eq!(encrypted.timestamp, 1687244806000000);

        let decrypted = encrypted.decrypt::<PASETO_V4>(&key).unwrap();

        assert_eq!(decrypted.data.0, [1, 2, 3, 4]);
        assert_eq!(decrypted.id, "1");
        assert_eq!(decrypted.host, "host1");
        assert_eq!(decrypted.version, "v0");
        assert_eq!(decrypted.tag, "kv");
        assert_eq!(decrypted.timestamp, 1687244806000000);
    }

    #[test]
    fn full_record_round_trip_fail() {
        let key = [0x55; 32];
        let record = Record::builder()
            .id("1".to_owned())
            .version("v0".to_owned())
            .tag("kv".to_owned())
            .host("host1".to_owned())
            .timestamp(1687244806000000)
            .data(DecryptedData(vec![1, 2, 3, 4]))
            .build();

        let encrypted = record.encrypt::<PASETO_V4>(&key);

        let mut enc1 = encrypted.clone();
        enc1.host = "host2".to_owned();
        let _ = enc1.decrypt::<PASETO_V4>(&key).expect_err("tampering with the host should result in auth failure");

        let mut enc2 = encrypted;
        enc2.id = "2".to_owned();
        let _ = enc2.decrypt::<PASETO_V4>(&key).expect_err("tampering with the id should result in auth failure");
    }
}
