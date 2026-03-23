# Batty Concept — Original Idea

inspect this project, inspect ./docs/new_beginnings docs and let's work on this concept. Currently I optimized solving mafia
game (in ~/mafia_solver) and this script - ~/bin/codex_telegram_watcher.py and I want to integrate it to batty, we probably do
heavy refactoring of batty but we need to understand how. Here's how I work currently I chat with clawdbot via telegram and ask
it to make changes in ~/mafia_solver/.batty/worker_cont.md and what this script is doing is every time codex finished task it
send me copy of that message in the telegram via clawdbot and text of worker_cont.md to tmux session where agent runs (single
copy)... I have better idea - that we can define an "org chart" via yaml files and .md files for each member of the team and
maybe common board for the team (using markdown md) and bidirectional communication between members of the team and
looping/scheduling for "pings". So here's my first crude idea how it might work - architect <-> manager <-> (5) engineers. So
when we start batty it will launch or restore tmux with 7 panes (screen split to 3 zones architect | manager | 5 engineers
(horizontal splits)) but we can define layout in yaml files. Then this is what happening - engineers are running in the non
stopping loop - our daemon detect when engineer stopped and send message from it to manager and manager send message to the
engineer (via CLI tool probably) if engineer is idle it poll manager for 5 minutes. manager can pause engineer and unpause with
commands. Engineers are empowered to use worktrees, git etc. Engineers writing code. Manager manage the board and as it give
task assignments to engineers from the board it have a context of the project from the board and messaging with engineers. Also
there's certain "chat zones" are organized - could be just .md files with some rotation by size. Then architect <-> manager -
architect talk to human through chatbot and getting polls every 30 minutes (could be set in his yaml file config) to make
progress on it's own with predefined system message nudge (like find good research directions, plan roadmap, experiments).
Architect own project documentation high level (it is open for everyone and engineers and managers couild update it but main
owner is architect) and it send messages to the manager to prioritize/deprioritize/add items to the board - manager own kanban
board for the project. Instead of many multiple boards we have one single board and completed items just being rotated out.
Architect can add/remove items itself or ask manager to do that - we do not want to constrain it but architect is main
character who actually talk to the human. So - thoughts? Feedback on this idea? let's discuss it
