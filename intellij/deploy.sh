#!/usr/bin/env bash
# Build the plugin and publish it, from WSL, to the folder on the Windows side that IntelliJ reads as a
# custom plugin repository: the one Hexagonal Inspector's deploy.sh uses, so one repository URL serves both.
# In IntelliJ, once:
#   Settings > Plugins > (gear) > Manage Plugin Repositories... > +
#   file:///C:/Users/<you>/hexagonal-inspector-repo/updatePlugins.xml
# Then after each ./deploy.sh: Settings > Plugins > Updates > Update.
set -euo pipefail
cd "$(dirname "$0")"

WIN_USER="${WISH_WIN_USER:-$(cmd.exe /c 'echo %USERNAME%' 2>/dev/null | tr -d '\r')}"
DEPLOY_DIR="${WISH_DEPLOY_DIR:-/mnt/c/Users/$WIN_USER/hexagonal-inspector-repo}"
mkdir -p "$DEPLOY_DIR"
DEPLOY_URL="file:///$(wslpath -m "$DEPLOY_DIR")"

./gradlew deploy --console=plain -q \
    -PbuildStamp="$(date +%Y%m%d%H%M%S)" \
    -PdeployDir="$DEPLOY_DIR" \
    -PdeployUrl="$DEPLOY_URL" "$@"

echo "Repository URL for IntelliJ: $DEPLOY_URL/updatePlugins.xml"
echo "Zip for Install Plugin from Disk: $(wslpath -w "$DEPLOY_DIR")\\wish.zip"
