"""Generate the graphrt orchestration-overhead workflows (stdlib only).

    python3 workflows/generate.py OUT_DIR

Writes, as JSON workflow documents (graphrt reads YAML or JSON):

- chain-N.json   start -> p0 -> ... -> p(N-1) -> end, every p a
                 `variable-aggregator` passing the previous value through.
- fanout-N.json  start -> {p0 .. p(N-1)} -> join -> end, every p passing the
                 start input through and `join` waiting for all of them.

The Python counterparts are `chain` and `fanout` in python/graphs.py. N is the
number of passthrough nodes; start/end/join are not counted.
"""

import json
import os
import sys

SIZES = (10, 1000)


def start_node():
    return {"id": "start", "type": "start", "variables": [{"variable": "x", "type": "text-input", "required": True}]}


def passthrough(node_id, source):
    return {"id": node_id, "type": "variable-aggregator", "variables": [[source[0], source[1]]]}


def chain(length):
    nodes, edges = [start_node()], []
    previous = ("start", "x")
    for index in range(length):
        node_id = f"p{index}"
        nodes.append(passthrough(node_id, previous))
        edges.append({"source": previous[0], "target": node_id})
        previous = (node_id, "output")
    nodes.append({"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": list(previous)}]})
    edges.append({"source": previous[0], "target": "end"})
    return {"version": "1", "name": f"chain-{length}", "graph": {"nodes": nodes, "edges": edges}}


def fanout(width):
    nodes = [start_node(), passthrough("join", ("p0", "output"))]
    edges = []
    for index in range(width):
        node_id = f"p{index}"
        nodes.append(passthrough(node_id, ("start", "x")))
        edges.append({"source": "start", "target": node_id})
        edges.append({"source": node_id, "target": "join"})
    nodes.append({"id": "end", "type": "end", "outputs": [{"variable": "out", "value_selector": ["join", "output"]}]})
    edges.append({"source": "join", "target": "end"})
    return {"version": "1", "name": f"fanout-{width}", "graph": {"nodes": nodes, "edges": edges}}


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    for size in SIZES:
        for name, build in (("chain", chain), ("fanout", fanout)):
            with open(os.path.join(out_dir, f"{name}-{size}.json"), "w") as handle:
                json.dump(build(size), handle)


if __name__ == "__main__":
    main()
