import { expect, mock, test } from 'claude-code/testing'

const FILE = '/rudder/signals/run_a-suggestion.json'

test('the file holds the suggestion while it shows, and null from the next turn', async ($, on) => {
  mock.env(on, { RUDDER_PROMPT_SUGGESTION_FILE: FILE })
  const clock = mock.clock(on, { now: 1000 })
  const writes: { path: string; text: string }[] = []
  on('fs.write', (_, e) => {
    writes.push({ path: e.path, text: e.text })
    return { value: undefined }
  })
  // Each hook here stands for core: the box shows the first, not the second.
  let boxHoldsText = false
  on('prompt.suggest', () => ({ isShown: !boxHoldsText }))
  on('turn.start', (_, e) => ({ turnId: e.turnId }))

  await $.prompt.suggest({ text: 'run the tests you just wrote', origin: { kind: 'suggestion' } })
  boxHoldsText = true
  await $.prompt.suggest({ text: 'commit it', origin: { kind: 'suggestion' } })
  await $.turn.start({ text: '', turnId: 'sub', agentId: 'agent-1' })
  await clock.advance(500)
  await $.turn.start({ text: 'run the tests you just wrote', turnId: 'main' })

  expect(writes).toEqual([
    { path: FILE, text: JSON.stringify({ text: 'run the tests you just wrote', at: 1000 }) },
    { path: FILE, text: JSON.stringify({ text: null, at: 1500 }) },
  ])
})
