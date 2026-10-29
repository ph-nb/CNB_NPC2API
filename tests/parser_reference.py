"""Compare the shared parser corpus against the checked-out ToolForge implementation."""

import json
import itertools
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(root / "toolforge"))
from app.engine.xyml import parse_tool_calls


def generated_cases(tools):
    samples = []
    for protocol, quote, cdata, value in itertools.product(
        ("XYML", "QNML"), ('"', "'", ""), (False, True), ("Paris", "Tokyo", "New York")
    ):
        if not quote and " " in value:
            continue
        open_invoke = f"<|{protocol}|invoke name={quote}get_weather{quote}>"
        open_param = f"<|{protocol}|parameter name={quote}city{quote}>"
        content = f"<![CDATA[{value}]]>" if cdata else value
        text = f"{open_invoke}{open_param}{content}</|{protocol}|parameter></|{protocol}|invoke>"
        samples.append(text)
        samples.append(f"before <|{protocol}|tool_calls>{text}</|{protocol}|tool_calls> after")
    for protocol, value in itertools.product(("XYML", "QNML"), ("Paris", "Tokyo", "New York")):
        samples.append(f"{protocol} name=\"get_weather\" parameter=\"city\" <![CDATA[{value}]]>")
        samples.append(f"<|{protocol}|invoke name=\"get_weather\">city={value}\nunits=metric</|{protocol}|invoke>")
    for wrapper, key, value in itertools.product(
        ("plain", "fenced", "surrounded"), ("name", "tool", "tool_name", "function_name"), ("Paris", "Tokyo")
    ):
        raw = json.dumps({key: "get_weather", "arguments": {"city": value}})
        samples.append(f"```json\n{raw}\n```" if wrapper == "fenced" else f"before {raw} after" if wrapper == "surrounded" else raw)
    result = []
    for index, text in enumerate(samples):
        calls = [{"name": call.name, "arguments": call.input} for call in parse_tool_calls(text, tools)]
        result.append({"name": f"generated-{index:03d}", "text": text, "calls": calls})
    return result


def main():
    corpus = json.loads((Path(__file__).parent / "parser_corpus.json").read_text(encoding="utf-8"))
    generated = generated_cases(corpus["tools"])
    generated_path = Path(__file__).parent / "parser_generated.json"
    if "--generate" in sys.argv[1:]:
        generated_path.write_text(json.dumps(generated, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    elif json.loads(generated_path.read_text(encoding="utf-8")) != generated:
        raise SystemExit("Generated corpus differs from the ToolForge reference")
    failures = []
    for case in corpus["cases"]:
        calls = [{"name": call.name, "arguments": call.input} for call in parse_tool_calls(case["text"], corpus["tools"])]
        if calls != case["calls"]:
            failures.append(case["name"])
    if failures:
        raise SystemExit("Reference parser mismatches: " + ", ".join(failures))
    print(f"ToolForge reference parser: {len(corpus['cases'])} curated and {len(generated)} generated cases passed")


if __name__ == "__main__":
    main()
