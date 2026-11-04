set -eu
python3 - <<'PY'
import plistlib,subprocess
from pathlib import Path
app=str(Path.home()/'Applications/else.bot.app')
x={'tile-data':{'file-data':{'_CFURLString':app,'_CFURLStringType':0},'file-label':'else.bot'},'tile-type':'file-tile'}
subprocess.run(['defaults','write','com.apple.dock','persistent-apps','-array-add',plistlib.dumps(x).decode()],check=True)
PY
killall Dock
