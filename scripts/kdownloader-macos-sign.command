#!/bin/bash
#
# kdownloader-macos-sign.command — make KDownloader's automatic UPnP work on macOS.
#
# KDownloader is distributed without Apple notarization. On macOS, an unsigned app's
# access to the local network (needed to talk to your router for UPnP port
# forwarding) is blocked by the system privacy layer, and an ad-hoc signature
# changes on every copy so the permission never sticks.
#
# This script gives KDownloader a *stable, locally-trusted* code signature so macOS
# will honor the "Local Network" permission you grant it. It is a ONE-TIME
# setup, done entirely on your own machine — nothing is uploaded anywhere.
#
# Usage:
#   ./kdownloader-macos-sign.command            # signs /Applications/KDownloader.app
#   ./kdownloader-macos-sign.command /path/to/KDownloader.app
#   ./kdownloader-macos-sign.command --remove   # undo: delete the local certificate
#
# Security note: this creates a self-signed certificate, trusted for CODE
# SIGNING on your account only. Its private key is generated locally and never
# leaves your Mac. Removing it later: run with --remove, or delete
# "KDownloader Local Signing" from Keychain Access.
#
set -euo pipefail

IDENTITY="KDownloader Local Signing"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

say() { printf '\033[1;34m==>\033[0m %s\n' "$1"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$1" >&2; exit 1; }

[[ "$(uname)" == "Darwin" ]] || die "This script is for macOS only."

# --- uninstall -------------------------------------------------------------
if [[ "${1:-}" == "--remove" ]]; then
  say "Removing the local signing certificate…"
  security delete-identity -c "$IDENTITY" "$KEYCHAIN" 2>/dev/null || true
  security delete-certificate -c "$IDENTITY" "$KEYCHAIN" 2>/dev/null || true
  echo "Done. (KDownloader stays signed with it until you rebuild/replace the app.)"
  exit 0
fi

APP="${1:-/Applications/KDownloader.app}"
[[ -d "$APP" ]] || die "App not found: $APP  (pass the path as the first argument)"

say "Signing: $APP"

# --- 1. create + trust a stable self-signed code-signing identity (once) ----
if security find-identity -v -p codesigning 2>/dev/null | grep -q "$IDENTITY"; then
  say "Reusing existing certificate: $IDENTITY"
else
  say "Creating a local code-signing certificate ($IDENTITY)…"
  TMP="$(mktemp -d)"
  trap 'rm -rf "$TMP"' EXIT
  # Config-file extensions: portable across both LibreSSL (/usr/bin/openssl,
  # the macOS default) and OpenSSL 3 (Homebrew). `-addext` is not.
  cat > "$TMP/cert.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $IDENTITY
[ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF
  openssl req -x509 -newkey rsa:2048 -keyout "$TMP/key.pem" -out "$TMP/cert.pem" \
    -days 3650 -nodes -config "$TMP/cert.cnf" >/dev/null 2>&1
  # OpenSSL 3+ defaults to a PKCS#12 MAC that macOS can't read; its `-legacy`
  # flags fix that. LibreSSL (macOS default) already uses the old algorithms and
  # has no such flag — so feature-detect it rather than guessing from version.
  P12_LEGACY=()
  if openssl pkcs12 -help 2>&1 | grep -q -- '-legacy'; then
    P12_LEGACY=(-legacy -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1)
  fi
  openssl pkcs12 -export -inkey "$TMP/key.pem" -in "$TMP/cert.pem" -out "$TMP/id.p12" \
    -passout pass:kdownloader -name "$IDENTITY" "${P12_LEGACY[@]}" >/dev/null 2>&1
  security import "$TMP/id.p12" -k "$KEYCHAIN" -P kdownloader -A -T /usr/bin/codesign >/dev/null
  say "Trusting it for code signing — enter your macOS login password if asked…"
  security add-trusted-cert -r trustRoot -p codeSign "$TMP/cert.pem"
fi

# --- 2. clear the download quarantine flag and sign ------------------------
say "Clearing quarantine flag and signing…"
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
codesign --force --sign "$IDENTITY" --timestamp=none "$APP"
codesign --verify --verbose "$APP" && say "Signature OK."

cat <<EOF

✅ Done. Now:
   1. Launch KDownloader.
   2. Allow it in: System Settings → Privacy & Security → Local Network → KDownloader
      (so it can open your BitTorrent port via UPnP, like other clients do).

That's it — the permission will stick from now on.
EOF
