//! A machine's own identity, which outlives the processes running on it.
//!
//! A node's peer id is generated when it binds its endpoint, so it belongs to
//! one process rather than to the box. That is right for the transport — two
//! nodes on one machine are two peers, which is exactly what every localhost
//! session needs — and wrong for a *fleet*, where the question is "is that the
//! droplet I paired six months ago" and the answer has to survive a reboot.
//!
//! So this is a second identity, carried alongside the peer id rather than
//! replacing it: one keypair per machine, written once into the same directory
//! as the pairing record, and proved rather than asserted. A member announces
//! its machine id together with a signature over the peer id the transport
//! already authenticated, so a peer can claim its own identity and nobody
//! else's — forging one means signing with a key you do not have, and replaying
//! one means also being the node it was issued for.
//!
//! Nothing here is a secret shared between machines. Every machine has its own
//! key and never sends it; what crosses the wire is a public key and a
//! signature, and what decides ownership is still a file only you can write.

use std::{fs, io, path::PathBuf};

use iroh::{PublicKey, SecretKey, Signature};

/// The file the machine's key lives in, beside the pairing record.
///
/// Beside it on purpose: the two answer the same question from opposite ends —
/// this is who I am, that is who I recognize — and a machine that lost one and
/// kept the other would be in a fleet it could not prove it belonged to.
pub fn key_path() -> Result<PathBuf, io::Error> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no HOME or XDG_CONFIG_HOME to store the machine key in",
            )
        })?;
    Ok(base.join("p2pmux").join("machine.key"))
}

/// This machine's key, created on first use.
///
/// Created rather than demanded: a machine that has never been paired still has
/// an identity, and generating it lazily means nothing has to be initialized
/// before p2pmux is useful.
pub fn load_or_create() -> Result<SecretKey, io::Error> {
    let path = key_path()?;
    match fs::read(&path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "machine key file is not a 32-byte key",
                )
            })?;
            Ok(SecretKey::from_bytes(&bytes))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut seed = [0_u8; 32];
            getrandom::fill(&mut seed)
                .map_err(|error| io::Error::other(format!("no randomness available: {error}")))?;
            let secret = SecretKey::from_bytes(&seed);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_private(&path, &secret.to_bytes())?;
            Ok(secret)
        }
        Err(error) => Err(error),
    }
}

/// This machine's public identity, or empty when it has none to offer.
///
/// Empty rather than an error: a machine that cannot write its config
/// directory should still be able to join a session. It simply cannot be
/// recognized as one of your machines, which is the safe way to fail.
pub fn machine_id() -> Vec<u8> {
    load_or_create()
        .map(|secret| secret.public().as_bytes().to_vec())
        .unwrap_or_default()
}

/// Prove that this machine is behind `peer_id`.
///
/// Signed over the peer id and nothing else, because the peer id is what the
/// transport authenticated: a signature that named a session as well would be
/// no harder to forge and one more thing for a reader to have on hand.
pub fn prove(peer_id: &[u8]) -> Vec<u8> {
    load_or_create()
        .map(|secret| secret.sign(peer_id).to_bytes().to_vec())
        .unwrap_or_default()
}

/// Whether `machine_id` really is the machine behind `peer_id`.
///
/// Everything downstream treats a machine id as either verified or absent, so
/// this is called once, where a member list is built, and never again.
pub fn verify(machine_id: &[u8], peer_id: &[u8], proof: &[u8]) -> bool {
    let (Ok(key), Ok(proof)) = (
        <[u8; 32]>::try_from(machine_id),
        <[u8; 64]>::try_from(proof),
    ) else {
        return false;
    };
    let Ok(key) = PublicKey::from_bytes(&key) else {
        return false;
    };
    key.verify(peer_id, &Signature::from_bytes(&proof)).is_ok()
}

/// The text form used in the pairing record and anywhere a machine has to be
/// named to another process.
pub fn to_hex(machine_id: &[u8]) -> String {
    machine_id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::{prove, to_hex, verify};

    /// The property the whole design rests on: a machine can claim its own
    /// identity and nobody else's.
    #[test]
    fn a_proof_only_verifies_for_the_peer_it_was_made_for() {
        let peer = b"peer-a-endpoint-key-thirty-two!!";
        let machine = super::machine_id();
        if machine.is_empty() {
            // No writable config directory in this environment; the failure
            // mode is "cannot be recognized", which the caller handles.
            return;
        }
        let proof = prove(peer);

        assert!(verify(&machine, peer, &proof));
        assert!(
            !verify(&machine, b"a-different-peers-endpoint-key!!", &proof),
            "a proof replayed under another peer id must not verify"
        );
        assert!(!verify(&machine, peer, b"not a signature"));
        assert!(!verify(b"not a key", peer, &proof));
        assert!(!to_hex(&machine).is_empty());
    }
}
