"""Make `eval/` importable regardless of how pytest is invoked.

`python run.py ...` works because Python puts the script's own directory on
sys.path. Tests should see the same `import run` / `from metrics.loudness import
...` regardless of whether they're run as `python -m pytest` (which already
prepends cwd) or a bare `pytest` from some other directory.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
