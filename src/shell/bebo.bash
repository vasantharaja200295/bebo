# bebo shell integration for bash. bebo starts bash with --rcfile set to this file.
# A Mac app gets a short PATH. /etc/profile and the login files of the user give the full PATH.
[ -r /etc/profile ] && . /etc/profile
if [ -r ~/.bash_profile ]; then . ~/.bash_profile
elif [ -r ~/.bash_login ]; then . ~/.bash_login
elif [ -r ~/.profile ]; then . ~/.profile
elif [ -r ~/.bashrc ]; then . ~/.bashrc
fi

# The marks: 133;D (exit code of the last command), 7717;cwd (folder), 133;A (prompt start), 133;B (command start), 133;C (command runs).
__bebo_prompt() {
  local code=$?
  printf '\e]133;D;%s\a\e]7717;cwd;%s\a\e]133;A\a' "$code" "$PWD"
}
PROMPT_COMMAND="__bebo_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
PS1="$PS1"'\[\e]133;B\a\]'
# PS0 shows before a command runs. bash 3.2 (the bash of macOS) does not have it, so it gives no duration there.
PS0=$'\e]133;C\a'
restart() { printf '\e]7717;restart\a'; }
