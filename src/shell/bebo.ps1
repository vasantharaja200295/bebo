# bebo shell integration for PowerShell 5.1 and 7.
# bebo sends this file as one line. End each statement with ";". Do not use double quotes or comments in the code part.
$global:__bebo_e = [string][char]27;
$global:__bebo_b = [string][char]7;
$global:__bebo_prompt = $function:prompt;
function global:prompt {
  $ok = $?;
  $code = 0;
  if (-not $ok) { if ($global:LASTEXITCODE) { $code = $global:LASTEXITCODE } else { $code = 1 } };
  $e = $global:__bebo_e;
  $b = $global:__bebo_b;
  $cwd = '';
  if ($PWD.Provider.Name -eq 'FileSystem') { $cwd = $e + ']7717;cwd;' + $PWD.ProviderPath + $b };
  $text = & $global:__bebo_prompt;
  $e + ']133;D;' + $code + $b + $cwd + $e + ']133;A' + $b + $text + $e + ']133;B' + $b
};
function global:restart { [Console]::Write($global:__bebo_e + ']7717;restart' + $global:__bebo_b) };
Import-Module PSReadLine -ErrorAction SilentlyContinue;
if (Get-Module PSReadLine) {
  function global:PSConsoleHostReadLine {
    $line = [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine($Host.Runspace, $ExecutionContext);
    if ($line.Trim()) { [Console]::Write($global:__bebo_e + ']133;C' + $global:__bebo_b) };
    $line
  }
};
