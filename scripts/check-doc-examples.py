"""Check syntax/API fields without sending any request to a model provider."""
import ast
import json
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib
from doc_tool_inputs import check as check_tool_inputs

ROOT = Path(__file__).resolve().parent.parent
manifest = json.loads((ROOT / 'docs/manifest.json').read_text())
files = [ROOT / entry['source'] for entry in manifest]
files += [ROOT / p for p in ['README.md', 'crates/mink-core/README.md', 'mink_agent/README.md']]
sdk = ast.parse((ROOT / 'mink_agent/__init__.py').read_text())
config = next(n for n in sdk.body if isinstance(n, ast.ClassDef) and n.name == 'SandboxConfig')
fields = {n.target.id for n in config.body if isinstance(n, ast.AnnAssign)}
cli = (ROOT / 'crates/mink-cli/src/cli.rs').read_text()
cli += (ROOT / 'crates/mink-cli/src/config.rs').read_text()
config_source = (ROOT / 'crates/mink-cli/src/config.rs').read_text()
struct_names = {
    'provider': 'ProviderConfigFile', 'provider.image': 'ImageConfigFile',
    'generation': 'GenerationConfigFile', 'context': 'ContextConfigFile',
    'tools': 'ToolsConfigFile', 'tools.edit': 'EditConfigFile',
    'signal': 'SignalPolicyFile', 'recovery': 'RecoveryConfigFile',
    'sandbox': 'SandboxConfigFile', 'sandbox_python': 'SandboxPythonConfigFile',
}
server_source = (ROOT / 'crates/mink-server/src/session/config.rs').read_text()
allowed = {}
for group, name in [('server', 'Server'), ('agent', 'Agent')]:
    body = re.search(r'    struct ' + name + r' \{(.*?)\n    \}', server_source, re.S)[1]
    allowed[group] = dict(re.findall(r'        (\w+): ([^\n]+),', body))
reference = (ROOT / 'docs/reference/configuration.md').read_text()
for group, name in struct_names.items():
    body = re.search(r'pub struct ' + name + r' \{(.*?)\n\}', config_source, re.S)[1]
    members = dict(re.findall(r'pub (\w+): ([^\n]+),', body))
    allowed[group] = members
    section = reference.split(f'### `[{group}]`\n', 1)[1].split('\n### ', 1)[0].split('\n## ', 1)[0]
    rows = dict(re.findall(r'^\| `(\w+)` \| `([^`]+)` \|$', section, re.M))
    assert rows == members, (group, 'TOML field index drift')
for field in fields:
    assert f'| `{field}` |' in reference.split('## Python 字段索引', 1)[1], field

protocol = (ROOT / 'docs/reference/protocols.md').read_text()
sdk_source = (ROOT / 'crates/mink-core/src/sdk_protocol.rs').read_text()
for name, body in re.findall(r'pub struct (Sdk\w+) \{(.*?)\n\}', sdk_source, re.S):
    fields_index = dict(re.findall(r'pub (\w+): ([^\n]+),', body))
    if name == 'SdkFinal':
        fields_index['type'] = fields_index.pop('event_type')
    section = protocol.split(f'### `{name}`\n', 1)[1].split('\n### ', 1)[0]
    assert dict(re.findall(r'^\| `(\w+)` \| `([^`]+)` \|$', section, re.M)) == fields_index, name

def validate_toml(table, group=''):
    if group == 'dependencies' or group.startswith('dependencies.') or group in ['provider.model_aliases', 'provider.openai_extra_body', 'tools.approval']:
        return
    for name, value in table.items():
        if group:
            assert name in allowed[group], (group, name)
        else:
            assert name in allowed or name in ['dependencies', 'server'], name
        if isinstance(value, dict):
            validate_toml(value, f'{group}.{name}' if group else name)

counts = dict(python=0, toml=0, cli=0, rust=0, tool_json=0)
rust = set()
for file in files:
    for language, code in re.findall(r'^```(\w+)\n(.*?)^```', file.read_text(), re.M | re.S):
        if language == 'python':
            tree = ast.parse(code, filename=str(file))
            for call in ast.walk(tree):
                if isinstance(call, ast.Call) and isinstance(call.func, ast.Name) and call.func.id == 'SandboxConfig':
                    for keyword in call.keywords:
                        assert keyword.arg in fields, (file, keyword.arg)
            counts['python'] += 1
        elif language == 'toml':
            try:
                validate_toml(tomllib.loads(code))
            except tomllib.TOMLDecodeError as error:
                raise ValueError(f'{file}: {error}') from error
            counts['toml'] += 1
        elif language in ['bash', 'sh', 'shell']:
            # Validate documented Mink CLI flags against the current parser text.
            for line in code.replace('\\\n', ' ').splitlines():
                if not re.search(r'(?:^|\|\s*)(?:\S*/)?mink(?:-core)?(?:\s|$)', line) or line.lstrip().startswith('#'):
                    continue
                for flag in re.findall(r'--[a-z][a-z-]*', line):
                    assert flag in cli, (file, flag)
                    counts['cli'] += 1
        elif language == 'rust' and ('async fn main()' in code or 'pub fn parse_assignment' in code):
            rust.add(code)
# Complete Rust examples get individual bins; API fragments stay documented as fragments.
with tempfile.TemporaryDirectory(prefix='mink-doc-examples-') as temporary:
    project = Path(temporary)
    (project / 'src/bin').mkdir(parents=True)
    (project / 'Cargo.toml').write_text(f'''[package]
name = "mink-doc-examples"
version = "0.0.0"
edition = "2024"
[dependencies]
mink = {{ package = "mink-core", path = "{ROOT / 'crates/mink-core'}", default-features = false, features = ["runtime"] }}
tokio = {{ version = "1", features = ["full"] }}
anyhow = "1"
serde_json = "1"
''')
    (project / 'src/bin/doc_tool_schemas.rs').write_text((ROOT / 'scripts/doc-tool-schemas.rs').read_text())
    for index, code in enumerate(sorted(rust)):
        if 'pub fn parse_assignment' in code:
            (project / 'src/lib.rs').write_text(code)
        else:
            (project / f'src/bin/example_{index}.rs').write_text(code)
    subprocess.run(['cargo', 'check', '--manifest-path', str(project / 'Cargo.toml'), '--all-targets', '--target-dir', str(ROOT / 'target/doc-examples')], check=True)
    subprocess.run(['cargo', 'test', '--manifest-path', str(project / 'Cargo.toml'), '--lib', '--target-dir', str(ROOT / 'target/doc-examples')], check=True)
    subprocess.run(['cargo', 'run', '--quiet', '--manifest-path', str(project / 'Cargo.toml'), '--bin', 'doc_tool_schemas', '--target-dir', str(ROOT / 'target/doc-examples'), '--', str(project)], check=True)
    schemas = {mode: {tool['name']: tool['input_schema'] for tool in json.loads((project / f'{mode}.json').read_text())} for mode in ['hashline', 'replace']}
    for file in files:
        counts['tool_json'] += check_tool_inputs(file.read_text(), schemas)
    assert counts['tool_json'] >= 3, 'expected both Edit modes and recovery envelope'
counts['rust'] = len(rust)
print('Documentation examples passed:', counts, '(syntax/API/fixture checks; no real model task)')
