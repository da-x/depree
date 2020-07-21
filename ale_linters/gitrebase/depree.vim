let s:exe_path = expand('<sfile>:p:h:h:h').'/bin/depree'

call ale#linter#Define('gitrebase', {
\   'name': 'depree',
\   'output_stream': 'stdout',
\   'executable': s:exe_path,
\   'command': s:exe_path.' verify-rebase-interactive %s',
\   'callback': 'ale#handlers#gcc#HandleGCCFormat',
\})
