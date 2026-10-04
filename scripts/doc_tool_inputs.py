"""Validate documented tool inputs against exported runtime JSON schemas."""
import json
import re


def validate(value, schema):
    kinds = {"object": dict, "array": list, "string": str, "integer": int, "boolean": bool, "number": (int, float), "null": type(None)}
    if "type" in schema:
        types = schema["type"] if isinstance(schema["type"], list) else [schema["type"]]
        assert any(isinstance(value, kinds[kind]) and not (kind in ["integer", "number"] and isinstance(value, bool)) for kind in types), (value, schema)
    if "enum" in schema:
        assert value in schema["enum"], value
    if isinstance(value, dict):
        properties = schema.get("properties", {})
        assert set(schema.get("required", [])) <= value.keys(), "missing required field"
        if schema.get("additionalProperties") is False:
            assert value.keys() <= properties.keys(), "unknown field"
        for key, item in value.items():
            if key in properties:
                validate(item, properties[key])
    if isinstance(value, list):
        assert len(value) >= schema.get("minItems", 0), "too few items"
        for item in value:
            if "items" in schema:
                validate(item, schema["items"])


def check(markdown, schemas):
    count = 0
    tool = None
    mode = "hashline"
    for match in re.finditer(r'^## `([^`]+)`[^\n]*$|^### (Hashline|Replace)[^\n]*$|^```json\n(.*?)^```', markdown, re.M | re.S):
        if match[1]:
            tool, mode = match[1], "hashline"
        elif match[2]:
            mode = match[2].lower()
        else:
            envelope = re.match(r'^\s*\{\s*"name"\s*:.*"arguments"\s*:', match[3], re.S)
            if tool not in schemas[mode] and not envelope:
                # JSONL protocol traces and other JSON documents are not tool
                # input examples; their own protocol checks remain separate.
                continue
            value = json.loads(match[3])
            name = value.get("name") if isinstance(value, dict) and "arguments" in value else tool
            if envelope:
                assert name in schemas[mode], f"unknown tool: {name}"
            if name in schemas[mode]:
                arguments = value["arguments"] if "arguments" in value else value
                validate(arguments, schemas[mode][name])
                count += 1
    return count
