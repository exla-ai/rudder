# Sourced by the iOS scripts: TEAM_ID and BUNDLE_ID from ios/project.yml, the
# one place the app's Apple team and bundle id are set
# (ios/scripts/appstore.mjs reads the same two lines).
_project="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/project.yml"
TEAM_ID=$(awk '/^    DEVELOPMENT_TEAM:/ {print $2; exit}' "$_project")
BUNDLE_ID=$(awk '/^  Rudder:/ {app = 1; next} /^  [^ ]/ {app = 0} app && /PRODUCT_BUNDLE_IDENTIFIER:/ {print $2; exit}' "$_project")
if [[ -z "$TEAM_ID" || -z "$BUNDLE_ID" ]]; then
  echo "ios/project.yml has no DEVELOPMENT_TEAM or no PRODUCT_BUNDLE_IDENTIFIER for the Rudder target" >&2
  exit 1
fi
unset _project
