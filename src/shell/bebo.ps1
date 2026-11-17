# bebo shell integration for PowerShell 5.1 and 7.
# After a resize, PSReadLine has a wrong row for the prompt. bebo then sends Ctrl+F12, and the key handler draws the prompt again.
# The handler does this only when the prompt and the command use one row. For two or more rows, the row of ConPTY and of bebo can be different by one.
# bebo sends this file as one line. End each statement with ";". Do not use double quotes or comments in the code part.
$global:__bebo_e = [string][char]27;
$global:__bebo_b = [string][char]7;
$global:__bebo_prompt = $function:prompt;
try { [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $global:OutputEncoding = [Console]::OutputEncoding } catch { };
function global:prompt {
  $ok = $?;
  $code = 0;
  if (-not $ok) { if ($global:LASTEXITCODE) { $code = $global:LASTEXITCODE } else { $code = 1 } };
  $e = $global:__bebo_e;
  $b = $global:__bebo_b;
  $cwd = '';
  if ($PWD.Provider.Name -eq 'FileSystem') { $cwd = $e + ']7717;cwd;' + $PWD.ProviderPath + $b };
  $text = & $global:__bebo_prompt;
  if ($text -eq ('PS ' + $PWD.Path + '> ')) { $text = $e + '[90mPS ' + $e + '[36m' + $PWD.Path + '>' + $e + '[0m ' };
  $e + ']133;D;' + $code + $b + $cwd + $e + ']133;A' + $b + $text + $e + ']133;B' + $b
};
function global:restart { [Console]::Write($global:__bebo_e + ']7717;restart' + $global:__bebo_b) };
Import-Module PSReadLine -ErrorAction SilentlyContinue;
if (Get-Module PSReadLine) {
  function global:PSConsoleHostReadLine {
    $line = [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine($Host.Runspace, $ExecutionContext);
    if ($line.Trim()) { [Console]::Write($global:__bebo_e + ']133;C' + $global:__bebo_b) };
    $line
  };
  try { Set-PSReadLineKeyHandler -Chord 'Ctrl+F12' -ScriptBlock {
    $line = $null;
    $cursor = $null;
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor);
    $e = $global:__bebo_e;
    $b = $global:__bebo_b;
    $p = [string](& $function:prompt);
    $p = ($p -replace ($e + '\][^' + $b + ']*' + $b), '') -replace ($e + '\[[0-9;?]*[A-Za-z]'), '';
    $one = ($p.Length + $line.Length) -lt [Console]::BufferWidth;
    if ($one -and -not ($line + $p).Contains([string][char]10)) { try { [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt($null, [Console]::CursorTop) } catch { } }
  } } catch { };
  $e = $global:__bebo_e;
  try { Set-PSReadLineOption -Colors @{ Command = $e + '[34m'; Parameter = $e + '[33m'; Operator = $e + '[39m'; String = $e + '[32m'; Number = $e + '[33m'; Variable = $e + '[31m'; Keyword = $e + '[35m'; Member = $e + '[36m'; Type = $e + '[36m'; Comment = $e + '[90m'; Default = $e + '[39m' } } catch { }
};
if ($env:BEBO_BANNER) {
  $parts = $env:BEBO_BANNER.Split('|');
  [Console]::WriteLine($global:__bebo_e + '[' + $parts[0] + 'm' + $parts[1] + $global:__bebo_e + '[0m');
  $env:BEBO_BANNER = $null
};
if ($env:BEBO_RUN) {
  $run = $env:BEBO_RUN;
  $env:BEBO_RUN = $null;
  Invoke-Expression $run
};
