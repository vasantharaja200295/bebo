# bebo shell integration for zsh. bebo starts zsh with ZDOTDIR set to the folder of this file (as .zshenv).
# The file puts ZDOTDIR back, so that zsh reads the files of the user after this one.
if [[ -n $BEBO_ZDOTDIR ]]; then ZDOTDIR=$BEBO_ZDOTDIR; else unset ZDOTDIR; fi
unset BEBO_ZDOTDIR
[[ -r ${ZDOTDIR:-$HOME}/.zshenv ]] && source ${ZDOTDIR:-$HOME}/.zshenv

if [[ -o interactive ]]; then
  autoload -Uz add-zsh-hook
  # The marks: 133;D (exit code of the last command), 7717;cwd (folder), 133;A (prompt start), 133;B (command start), 133;C (command runs).
  __bebo_first() {
    local code=$?
    printf '\e]133;D;%s\a\e]7717;cwd;%s\a\e]133;A\a' $code $PWD
  }
  # A prompt theme can set PS1 in its own precmd function. This function runs last, so the mark stays at the end of PS1.
  # ponytail: a theme that sets PS1 in a zle widget (after precmd) removes the mark. Fix: wrap zle-line-init.
  __bebo_last() {
    [[ $PS1 == *133\;B* ]] || PS1=$PS1$'%{\e]133;B\a%}'
    precmd_functions=(__bebo_first ${precmd_functions:#__bebo_*} __bebo_last)
  }
  __bebo_exec() { printf '\e]133;C\a' }
  add-zsh-hook precmd __bebo_first
  add-zsh-hook precmd __bebo_last
  add-zsh-hook preexec __bebo_exec
  restart() { printf '\e]7717;restart\a' }
fi
