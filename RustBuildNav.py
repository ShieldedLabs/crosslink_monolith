import N10X
import os
import re

# Rust build-output navigation for 10x.
#
# Supports two rust diagnostic layouts:
#   1) Human / multiline:
#        error[E0063]: ...
#           --> path/to/file.rs:374:37
#   2) Short / one-line:
#        path/to/file.rs:374:37: error[E0063]: ...
#
# Intended to be bound to F8 / Shift+F8 (and optionally F4 / Shift+F4).
# If no Rust results are found it gracefully falls back to the built-in 10x
# command, mirroring the style of the user's JaiGotoDefinition helper.

_SHORT_RE = re.compile(
    r'^\s*(.+):(\d+):(\d+):\s+(error|warning)(?:\[[^\]]+\])?:\s*(.*)$'
)
_KIND_RE = re.compile(r'^\s*(error|warning)(?:\[[^\]]+\])?:\s*(.*)$')
_ARROW_RE = re.compile(r'^\s*-->\s+(.+):(\d+):(\d+)\s*$')

_CACHE = {
    'signature': None,
    'results': [],
    'next_any': -1,
    'next_error': -1,
    'next_warning': -1,
}


def _reset_nav_state():
    _CACHE['next_any'] = -1
    _CACHE['next_error'] = -1
    _CACHE['next_warning'] = -1


def _workspace_dir():
    workspace_filename = N10X.Editor.GetWorkspaceFilename()
    if workspace_filename:
        return os.path.dirname(workspace_filename)
    return ''


def _normalise_path(path):
    path = path.strip().strip('"')
    if not path:
        return path

    is_windows_abs = len(path) >= 3 and path[1] == ':' and (path[2] == '\\' or path[2] == '/')
    is_unc = path.startswith('\\\\')
    is_unix_abs = path.startswith('/')

    if not (is_windows_abs or is_unc or is_unix_abs):
        workspace_dir = _workspace_dir()
        if workspace_dir:
            path = os.path.join(workspace_dir, path)

    return os.path.normpath(path)


def _append_result(results, seen, kind, filename, line_1, col_1, message):
    try:
        line_0 = max(int(line_1) - 1, 0)
        col_0 = max(int(col_1) - 1, 0)
    except ValueError:
        return

    filename = _normalise_path(filename)
    key = (kind, filename, line_0, col_0)
    if key in seen:
        return

    seen.add(key)
    results.append({
        'kind': kind,
        'filename': filename,
        'pos': (col_0, line_0),
        'message': message,
    })


def _parse_results(build_output):
    results = []
    seen = set()

    pending_kind = None
    pending_message = ''
    pending_ttl = 0

    for raw_line in build_output.splitlines():
        short_match = _SHORT_RE.match(raw_line)
        if short_match:
            _append_result(
                results,
                seen,
                short_match.group(4),
                short_match.group(1),
                short_match.group(2),
                short_match.group(3),
                short_match.group(5),
            )
            pending_kind = None
            pending_message = ''
            pending_ttl = 0
            continue

        kind_match = _KIND_RE.match(raw_line)
        if kind_match:
            pending_kind = kind_match.group(1)
            pending_message = kind_match.group(2)
            pending_ttl = 12
            continue

        if pending_kind is not None:
            arrow_match = _ARROW_RE.match(raw_line)
            if arrow_match:
                _append_result(
                    results,
                    seen,
                    pending_kind,
                    arrow_match.group(1),
                    arrow_match.group(2),
                    arrow_match.group(3),
                    pending_message,
                )
                pending_kind = None
                pending_message = ''
                pending_ttl = 0
                continue

            pending_ttl -= 1
            if pending_ttl <= 0:
                pending_kind = None
                pending_message = ''
                pending_ttl = 0

    return results


def _refresh_cache():
    try:
        build_output = N10X.Editor.GetBuildOutput()
    except Exception:
        build_output = ''

    if build_output is None:
        build_output = ''

    if build_output == _CACHE['signature']:
        return

    _CACHE['signature'] = build_output
    _CACHE['results'] = _parse_results(build_output)
    _reset_nav_state()


def _filtered_results(kind_filter):
    _refresh_cache()
    if kind_filter is None:
        return _CACHE['results']
    return [result for result in _CACHE['results'] if result['kind'] == kind_filter]


def _current_result_index(results):
    current_filename = _normalise_path(N10X.Editor.GetCurrentFilename())
    current_pos = N10X.Editor.GetCursorPos()
    current_line = current_pos[1]

    for i, result in enumerate(results):
        if result['filename'] == current_filename and result['pos'][1] == current_line:
            return i
    return -1


def _open_result(result):
    N10X.Editor.OpenFile(result['filename'])
    N10X.Editor.SetCursorPos(result['pos'])


def _goto_result(direction, kind_filter, fallback_command, cache_slot):
    results = _filtered_results(kind_filter)
    if not results:
        N10X.Editor.ExecuteCommand(fallback_command)
        return

    current_index = _current_result_index(results)
    cached_index = _CACHE[cache_slot]

    if current_index >= 0:
        next_index = (current_index + direction) % len(results)
    elif 0 <= cached_index < len(results):
        next_index = (cached_index + direction) % len(results)
    else:
        next_index = 0 if direction > 0 else len(results) - 1

    _CACHE[cache_slot] = next_index
    _open_result(results[next_index])


def RustGotoNextBuildWarningOrError():
    _goto_result(+1, None, 'GotoNextBuildWarningOrError', 'next_any')


def RustGotoPrevBuildWarningOrError():
    _goto_result(-1, None, 'GotoPrevBuildWarningOrError', 'next_any')


def RustGotoNextBuildError():
    _goto_result(+1, 'error', 'GotoNextBuildError', 'next_error')


def RustGotoPrevBuildError():
    _goto_result(-1, 'error', 'GotoPrevBuildError', 'next_error')


def RustGotoNextBuildWarning():
    _goto_result(+1, 'warning', 'GotoNextBuildWarning', 'next_warning')


def RustGotoPrevBuildWarning():
    _goto_result(-1, 'warning', 'GotoPrevBuildWarning', 'next_warning')


def RustGotoNextResult():
    _goto_result(+1, None, 'GotoNextResult', 'next_any')


def RustGotoPrevResult():
    _goto_result(-1, None, 'GotoPrevResult', 'next_any')


def _on_build_finished(_build_result):
    # Force a reparse on the next navigation after every build.
    _CACHE['signature'] = None
    _CACHE['results'] = []
    _reset_nav_state()


N10X.Editor.AddBuildFinishedFunction(_on_build_finished)
