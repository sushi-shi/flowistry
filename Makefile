.PHONY: test test-ui test-live test-precision test-save
test:
	nvim --headless -u NONE -i NONE -l tests/run.lua

test-ui:
	nvim --headless -n -i NONE --cmd 'let g:coc_start_at_startup=0' -c 'luafile tests/ui.lua'

test-live:
	nvim --headless -u NONE -i NONE -l tests/live.lua

test-precision:
	nvim --headless -u NONE -i NONE -l tests/precision.lua

test-save:
	nvim --headless -n -i NONE --cmd 'let g:coc_start_at_startup=0' -c 'luafile tests/save.lua'
