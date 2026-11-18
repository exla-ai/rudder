import type { EngineInterface, Register } from 'claude-code'

// Claude Code guesses the person's next prompt after a turn and shows it dim in
// its prompt box, but saves it nowhere. Rudder starts each Claude agent with
// this plugin (--plugin-dir, native/src/prompt_suggestion.rs) and sets
// RUDDER_PROMPT_SUGGESTION_FILE to a file of that agent's. The file holds
// {"text", "at"} while the box shows a suggestion and {"text": null} once a
// turn starts or the session ends; the Rudder app shows it in its own composer
// (cloud/worker/suggestion.mjs). A hook that fails is skipped and reported by
// name, so a failed write never holds up the prompt or the turn.

async function save($: EngineInterface, text: string | null) {
  const file = await $.env.get('RUDDER_PROMPT_SUGGESTION_FILE')
  if (!file) throw new Error('RUDDER_PROMPT_SUGGESTION_FILE is not set')
  await $.fs.write(file, JSON.stringify({ text, at: await $.clock.now() }))
}

export const register: Register = on => {
  on('prompt.suggest', async ($, e, next) => {
    const shown = await next(e)
    if (shown.isShown) await save($, e.text)
    return shown
  })
  on('turn.start', async ($, e, next) => {
    // A subagent's turns leave the person's prompt box alone.
    if (e.agentId === undefined) await save($, null)
    return next(e)
  })
  on('session.start', async ($, e, next) => {
    await save($, null)
    return next(e)
  })
  on('session.end', async ($, e, next) => {
    await save($, null)
    return next(e)
  })
}
