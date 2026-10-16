// Deterministic local OpenAI-compatible provider for real server/browser tests.
import { createServer } from 'node:http';

const failedReviewFixtures = new Set();
const failedExecutionFixtures = new Set();

function toolNames(parsed) {
  return new Set((parsed.tools ?? []).map(tool => tool.function?.name).filter(Boolean));
}

function lastToolCall(messages, userIndex) {
  let name;
  for (const message of messages.slice(userIndex + 1)) {
    if (message.role !== 'assistant') continue;
    for (const call of message.tool_calls ?? []) name = call.function?.name ?? name;
  }
  return name;
}

function lastToolOutput(messages, userIndex) {
  for (const message of [...messages.slice(userIndex + 1)].reverse()) {
    if (message.role !== 'tool') continue;
    const content = typeof message.content === 'string'
      ? message.content
      : (message.content ?? []).map(part => part.text ?? '').join('\n');
    try { return JSON.parse(content); } catch { /* not a structured tool result */ }
  }
  return undefined;
}

function goalSnapshot(output) {
  const snapshot = output?.draft ?? output?.goal;
  return snapshot && typeof snapshot === 'object' ? snapshot : undefined;
}

function emitTool(response, name, args) {
  response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: `fixture-${Date.now()}`, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] })}\n\n`);
}

function emitText(response, text) {
  response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: text }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
}

function sendUsage(response) {
  response.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 100, completion_tokens: 20, prompt_tokens_details: { cached_tokens: 60, cache_creation: 10 } } })}\n\n`);
  response.write('data: [DONE]\n\n');
  response.end();
}

createServer(async (request, response) => {
  if (request.method === 'GET') { response.end('ok'); return; }
  let body = '';
  for await (const chunk of request) body += chunk;
  const parsed = JSON.parse(body);
  const users = parsed.messages.filter(message => message.role === 'user');
  const last = users.at(-1)?.content;
  const text = typeof last === 'string' ? last : JSON.stringify(last);
  const allText = JSON.stringify(parsed.messages);
  const names = toolNames(parsed);
  const userIndex = parsed.messages.map(message => message.role).lastIndexOf('user');
  const lastCall = lastToolCall(parsed.messages, userIndex);
  const priorOutput = lastToolOutput(parsed.messages, userIndex);

  if (text.includes('fail-permanently')) {
    response.writeHead(400, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'fixture permanent failure', type: 'invalid_request_error' } }));
    return;
  }

  if (text.includes('retry-execution-fixture') && text.includes('<current-goal>') && !failedExecutionFixtures.has('retry-execution-fixture')) {
    failedExecutionFixtures.add('retry-execution-fixture');
    response.writeHead(400, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'fixture execution unavailable once', type: 'invalid_request_error' } }));
    return;
  }

  if (text.includes('<independent-goal-review>') && allText.includes('retry-review-fixture') && !failedReviewFixtures.has('retry-review-fixture')) {
    failedReviewFixtures.add('retry-review-fixture');
    response.writeHead(400, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'fixture reviewer unavailable once', type: 'invalid_request_error' } }));
    return;
  }

  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (text.includes('slow-task')) {
    response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta: { reasoning_content: 'Fixture request is in flight.' }, finish_reason: null }] })}\n\n`);
    await new Promise(resolve => setTimeout(resolve, 3500));
  }

  if (names.has('GoalDraft') && text.includes('<goal-draft>')) {
    if (lastCall === 'GoalDraft') {
      emitText(response, 'The draft contract is ready for human confirmation.');
    } else if (lastCall !== 'GoalRead') {
      emitTool(response, 'GoalRead', {});
    } else {
      const draft = goalSnapshot(priorOutput);
      const feedback = text.match(/Feedback: ([^\n]+)/)?.[1]?.trim() ?? '';
      const refined = feedback.includes('include the README heading');
      emitTool(response, 'GoalDraft', {
        draft_id: draft?.id ?? 'missing-draft-id',
        revision: draft?.revision ?? 0,
        spec: {
          objective: refined ? 'Verify the fixture README and its heading' : 'Verify the fixture README is readable',
          scope: refined ? 'README.md, including its heading' : 'README.md',
          constraints: [],
          subgoals: [{ id: 'inspect-readme', objective: 'Inspect the fixture README' }],
          criteria: [{
            id: 'readme-heading',
            subgoal_id: 'inspect-readme',
            outcome: refined ? 'README.md contains the fixture project heading' : 'README.md is readable',
            required_evidence: 'Read README.md',
          }],
          suggested_actions: ['Read README.md'],
        },
      });
    }
  } else if (text.includes('<independent-goal-review>')) {
    if (text.includes('partial-review-fixture') && !text.includes('Full evidence review required: true')) {
      emitText(response, 'The host explicitly leaves criterion-1 incomplete and will finish it next. Full evidence verification was not performed.');
    } else if (lastCall !== 'Read') {
      emitTool(response, 'Read', { path: 'README.md' });
    } else {
      emitText(response, 'I inspected README.md with Read. Its fixture project heading is present, supporting the original request, readme-heading criterion and the stated scope. No missing deliverables or external blockers were observed.');
    }
  } else if (names.has('GoalReview') && text.includes('<goal-review>')) {
    if (text.includes('partial-review-fixture')) {
      emitTool(response, 'GoalReview', { decision: 'continue', summary: 'The independent report identifies necessary work remaining.', remaining_requirements: ['criterion-1'], next_step: 'Implement the remainder of the fixture deliverable.' });
    } else {
      emitTool(response, 'GoalReview', { decision: 'complete', summary: 'The independent reviewer inspected README.md and confirmed the contract.' });
    }
  } else if (text.includes('partial-review-fixture')) {
    emitText(response, 'Only part of criterion-1 is implemented. Next implement the remaining required deliverable. The whole Goal is not complete.');
  } else {
    const image = text.match(/\[Attached image: "([^"]+)"\]/);
    const completedTool = lastCall !== undefined;
    const tool = (image || text.includes('read-fixture')) && !completedTool;
    if (tool) {
      emitText(response, 'Inspecting the requested file.');
      emitTool(response, 'Read', { path: image?.[1] ?? 'README.md' });
    } else {
      emitText(response, `Completed: ${text.includes('guidance') ? 'guidance applied' : 'fixture response'}`);
    }
  }

  sendUsage(response);
}).listen(Number(process.env.MINK_E2E_PROVIDER_PORT ?? 18822), '127.0.0.1');
