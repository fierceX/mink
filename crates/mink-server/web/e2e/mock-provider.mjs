// Deterministic local OpenAI-compatible provider for real server/browser tests.
import { createServer } from 'node:http';
createServer(async (request, response) => {
  if (request.method === 'GET') { response.end('ok'); return; }
  let body = ''; for await (const chunk of request) body += chunk;
  const parsed = JSON.parse(body);
  const users = parsed.messages.filter(message => message.role === 'user');
  const last = users.at(-1)?.content;
  const text = typeof last === 'string' ? last : JSON.stringify(last);
  if (text.includes('fail-permanently')) { response.writeHead(400, { 'content-type': 'application/json' }); response.end(JSON.stringify({ error: { message: 'fixture permanent failure', type: 'invalid_request_error' } })); return; }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (text.includes('slow-task')) {
    response.write(`data: ${JSON.stringify({choices:[{index:0,delta:{reasoning_content:'Fixture request is in flight.'},finish_reason:null}]})}\n\n`);
    await new Promise(resolve => setTimeout(resolve, 3500));
  }
  const userIndex = parsed.messages.map(message => message.role).lastIndexOf('user');
  const completedTool = parsed.messages.slice(userIndex + 1).some(message => message.role === 'tool');
  const image = text.match(/\[Attached image: "([^"]+)"\]/);
  const tool = (image || text.includes('read-fixture')) && !completedTool;
  const emit = (delta, finish_reason = null) => response.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason }] })}\n\n`);
  if (tool) {
    emit({ reasoning_content: 'Inspecting the requested file.' });
    emit({ tool_calls: [{ index: 0, id: `read-${Date.now()}`, type: 'function', function: { name: 'Read', arguments: JSON.stringify({ path: image?.[1] ?? 'README.md' }) } }] });
    emit({}, 'tool_calls');
  } else {
    emit({ content: `Completed: ${text.includes('guidance') ? 'guidance applied' : 'fixture response'}` });
    emit({}, 'stop');
  }
  response.write('data: [DONE]\n\n'); response.end();
}).listen(18822, '127.0.0.1');
