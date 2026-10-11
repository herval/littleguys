#!/usr/bin/env bash
# One-time setup for .github/workflows/release.yml. Puts the signing, notarization and
# updater credentials in this repo's GitHub secrets, and gives the workflow a deploy key
# that can push the cask to herval/homebrew-tap. Run it again to replace any of them;
# it asks before overwriting a secret that's already set.
#
#   make release-secrets
#
# Needs gh logged in as an admin of both repos, and a paid Apple Developer account.
#
# Developer ID certificate: with none in the login keychain, it makes a key and a signing
# request, opens developer.apple.com for you to turn that into a certificate, and picks the
# certificate up from ~/Downloads. The key stays in your login keychain (the GitHub secret
# can't be read back), so local builds can sign with it too. Already have a Developer ID
# certificate exported as a .p12? P12=path/to/it.p12 make release-secrets

set -euo pipefail

REPO=herval/littleguys
TAP=herval/homebrew-tap
DEPLOY_KEY_TITLE="littleguys release workflow"
UPDATER_KEY="$HOME/.tauri/littleguys-updater.key"
OPENSSL=/usr/bin/openssl # LibreSSL: the .p12 files it writes are ones `security import` reads
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if [ -t 1 ]; then B=$'\033[1m' P=$'\033[1;35m' R=$'\033[1;31m' X=$'\033[0m'; else B="" P="" R="" X=""; fi
say() { printf '\n%s==>%s %s%s%s\n' "$P" "$X" "$B" "$*" "$X"; }
note() { printf '    %s\n' "$*"; }
die() { printf '%serror:%s %s\n' "$R" "$X" "$*" >&2; exit 1; }
ask() { local reply; read -r -p "    $1 " reply; printf '%s' "$reply"; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

command -v gh >/dev/null || die "gh not found: brew install gh"
gh auth status >/dev/null 2>&1 || die "gh isn't logged in: gh auth login"
[ -f "$UPDATER_KEY" ] || die "No updater key at $UPDATER_KEY"

EXISTING="$(gh secret list -R "$REPO" --json name -q '.[].name')"

# put NAME: stores stdin as the secret NAME (never on the command line).
put() { gh secret set "$1" -R "$REPO" >/dev/null && note "set $1"; }

# wanted NAME...: true when any of them is missing, or you say to replace them.
wanted() {
  local name all=1
  for name in "$@"; do grep -qx "$name" <<<"$EXISTING" || all=0; done
  [ "$all" = 0 ] && return 0
  [[ "$(ask "$* already set. Replace? [y/N]")" =~ ^[Yy] ]]
}

subject_field() { $OPENSSL x509 -in "$1" -noout -subject -nameopt multiline | sed -n "s/^ *$2 *= //p"; }

# --- Developer ID certificate ------------------------------------------------------------

# Waits for the certificate Apple hands out to land in ~/Downloads, or for a path typed in.
wait_for_cer() {
  local since="$1" found typed
  while :; do
    found="$(find "$HOME/Downloads" -maxdepth 1 -name 'developerID_application*.cer' -newer "$since" 2>/dev/null | head -1)"
    [ -n "$found" ] && { printf '%s' "$found"; return; }
    if read -r -t 2 typed && [ -n "$typed" ]; then
      typed="${typed% }" typed="${typed//\\ / }" typed="${typed%\'}" typed="${typed#\'}" # as dragged in
      [ -f "$typed" ] && { printf '%s' "$typed"; return; }
      note "No file at $typed" >&2
    fi
  done
}

# Fills $WORK/cert.p12, $P12_PASSWORD and $CERT_PEM, from $P12 or a new certificate.
make_p12() {
  if [ -n "${P12:-}" ]; then
    [ -f "$P12" ] || die "No file at $P12"
    read -r -s -p "    Password for $P12: " P12_PASSWORD && echo
    cp "$P12" "$WORK/cert.p12"
    CERT_PEM="$WORK/cert.pem"
    $OPENSSL pkcs12 -in "$WORK/cert.p12" -passin env:P12_PASSWORD -nokeys -clcerts 2>/dev/null |
      $OPENSSL x509 -out "$CERT_PEM" 2>/dev/null || die "Couldn't open $P12 with that password."
    return
  fi

  local existing
  existing="$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: .*\)"$/\1/p' | head -1)"
  if [ -n "$existing" ]; then
    note "The login keychain already has \"$existing\"."
    note "Export it from Keychain Access (My Certificates, right-click, Export) as a .p12 and run"
    note "  P12=path/to/it.p12 make release-secrets"
    note "or press Enter to make a new certificate instead (an account can have up to 5)."
    read -r
  fi

  local csr="$HOME/Downloads/littleguys.certSigningRequest" cer issuer
  $OPENSSL req -new -newkey rsa:2048 -nodes -keyout "$WORK/key.pem" -out "$csr" \
    -subj "/CN=littleguys release" 2>/dev/null
  say "Get a Developer ID Application certificate from Apple"
  note "1. On the page that just opened, pick \"Developer ID Application\" (G2 Sub-CA), Continue."
  note "2. Upload ~/Downloads/littleguys.certSigningRequest, Continue."
  note "3. Download. This picks up developerID_application.cer from ~/Downloads by itself"
  note "   (or drag the .cer here and press Enter)."
  note "Only the Account Holder can make Developer ID certificates."
  open "https://developer.apple.com/account/resources/certificates/add"
  cer="$(wait_for_cer "$csr")"
  note "Got $cer"

  CERT_PEM="$WORK/cert.pem"
  $OPENSSL x509 -inform DER -in "$cer" -out "$CERT_PEM" 2>/dev/null ||
    $OPENSSL x509 -in "$cer" -out "$CERT_PEM" 2>/dev/null || die "$cer isn't a certificate."
  [ "$($OPENSSL x509 -in "$CERT_PEM" -noout -modulus)" = "$($OPENSSL rsa -in "$WORK/key.pem" -noout -modulus 2>/dev/null)" ] ||
    die "$cer wasn't made from littleguys.certSigningRequest. Upload that one."

  # The intermediate CA rides along in the .p12, so CI can build the chain without Xcode's copy.
  issuer="$($OPENSSL x509 -in "$CERT_PEM" -noout -text | sed -n 's/.*CA Issuers - URI:\(http[^ ]*\).*/\1/p' | head -1)"
  [ -n "$issuer" ] || die "$cer doesn't say who issued it."
  curl -fsSL "$issuer" | $OPENSSL x509 -inform DER -out "$WORK/ca.pem" || die "Couldn't fetch $issuer"

  P12_PASSWORD="$($OPENSSL rand -base64 24)"
  export P12_PASSWORD
  $OPENSSL pkcs12 -export -inkey "$WORK/key.pem" -in "$CERT_PEM" -certfile "$WORK/ca.pem" \
    -name "$(subject_field "$CERT_PEM" commonName)" -passout env:P12_PASSWORD -out "$WORK/cert.p12"
  security import "$WORK/cert.p12" -k "$KEYCHAIN" -P "$P12_PASSWORD" -T /usr/bin/codesign >/dev/null
  note "Added it to your login keychain"
  rm -f "$csr" "$cer"
}

TEAM_ID=""
say "Developer ID certificate"
if wanted APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY APPLE_TEAM_ID; then
  export P12_PASSWORD=""
  make_p12
  identity="$(subject_field "$CERT_PEM" commonName)"
  TEAM_ID="$(subject_field "$CERT_PEM" organizationalUnitName)"
  [[ "$identity" == "Developer ID Application: "* ]] ||
    die "That's a \"$identity\" certificate. Releases need a Developer ID Application one."
  note "$identity"
  base64 -i "$WORK/cert.p12" | tr -d '\n' | put APPLE_CERTIFICATE
  printf '%s' "$P12_PASSWORD" | put APPLE_CERTIFICATE_PASSWORD
  printf '%s' "$identity" | put APPLE_SIGNING_IDENTITY
  printf '%s' "$TEAM_ID" | put APPLE_TEAM_ID
else
  TEAM_ID="$(security find-identity -v -p codesigning | sed -n 's/.*"Developer ID Application: .*(\([A-Z0-9]\{10\}\))"$/\1/p' | head -1)"
fi

# --- Notarization ------------------------------------------------------------------------

say "Notarization"
if wanted APPLE_ID APPLE_PASSWORD; then
  [ -n "$TEAM_ID" ] || TEAM_ID="$(ask "Team ID (10 characters, developer.apple.com/account → Membership):")"
  note "Apple notarizes with an app-specific password: account.apple.com → Sign-In and Security"
  note "→ App-Specific Passwords. Opening it now."
  open "https://account.apple.com"
  default_id="$(git config user.email || true)"
  while :; do
    apple_id="$(ask "Apple Account email [$default_id]:")"
    apple_id="${apple_id:-$default_id}"
    read -r -s -p "    App-specific password: " apple_password && echo
    xcrun --find notarytool >/dev/null 2>&1 || { note "(no notarytool here to check them with)"; break; }
    xcrun notarytool history --apple-id "$apple_id" --password "$apple_password" --team-id "$TEAM_ID" >/dev/null 2>&1 && break
    note "Apple didn't accept those for team $TEAM_ID. Try again."
  done
  printf '%s' "$apple_id" | put APPLE_ID
  printf '%s' "$apple_password" | put APPLE_PASSWORD
fi

# --- Updater -----------------------------------------------------------------------------

say "Update signing key ($UPDATER_KEY)"
if wanted TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD; then
  put TAURI_SIGNING_PRIVATE_KEY <"$UPDATER_KEY"
  put TAURI_SIGNING_PRIVATE_KEY_PASSWORD <"$UPDATER_KEY.password"
fi

# --- Homebrew tap ------------------------------------------------------------------------

say "Deploy key for $TAP"
if wanted HOMEBREW_TAP_DEPLOY_KEY; then
  for id in $(gh repo deploy-key list -R "$TAP" --json id,title -q ".[] | select(.title == \"$DEPLOY_KEY_TITLE\") | .id"); do
    gh repo deploy-key delete "$id" -R "$TAP" && note "removed the old one"
  done
  ssh-keygen -q -t ed25519 -N "" -C "$DEPLOY_KEY_TITLE" -f "$WORK/tap"
  gh repo deploy-key add "$WORK/tap.pub" -R "$TAP" --allow-write --title "$DEPLOY_KEY_TITLE" >/dev/null
  note "added a write deploy key to $TAP"
  put HOMEBREW_TAP_DEPLOY_KEY <"$WORK/tap"
fi

say "Done. Secrets on $REPO:"
gh secret list -R "$REPO"
note ""
note "To release: set the version in src-tauri/tauri.conf.json, commit, then"
note "  git tag vX.Y.Z && git push origin main vX.Y.Z"
