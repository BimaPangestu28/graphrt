"""Python counterparts of the graphrt workflows in ../workflows/.

Every builder returns a compiled, reusable runnable whose `ainvoke({"x": ...})`
(or `invoke` for the sync variants) performs one run:

- llm3         prepare -> llm -> finish: one streamed chat completion against the
               fake OpenAI server (same 10-delta SSE body graphrt receives).
- chain-N      N passthrough nodes in sequence.
- fanout-N     N passthrough branches in parallel, joined by one node that waits
               for all of them.

Frameworks: `langgraph` (StateGraph, async nodes, `ainvoke`), `langgraph-sync`
(StateGraph, sync nodes, `invoke`: often faster for CPU-only nodes because async
graphs pay for the event loop) and `lcel` (plain LangChain Expression Language
runnables). Imports are local so a cold start loads only one framework.
"""

import operator
import os
from typing import Annotated, Any, TypedDict

SYSTEM_PROMPT = "You are a concise assistant."


def make_chat_model():
    """ChatOpenAI pointed at the fake server: streaming, no retries, no tracing."""
    from langchain_openai import ChatOpenAI

    return ChatOpenAI(
        model="gpt-4o-mini",
        base_url=os.environ["OPENAI_BASE_URL"],
        api_key=os.environ.get("OPENAI_API_KEY", "sk-fake"),
        streaming=True,
        max_retries=0,
    )


class LlmState(TypedDict, total=False):
    x: str
    messages: list
    text: str
    answer: str


class ChainState(TypedDict):
    x: str


class FanoutState(TypedDict, total=False):
    x: str
    outs: Annotated[list, operator.add]


def langgraph_llm3():
    from langchain_core.messages import HumanMessage, SystemMessage
    from langgraph.graph import END, START, StateGraph

    model = make_chat_model()

    async def prepare(state: LlmState) -> dict[str, Any]:
        return {"messages": [SystemMessage(SYSTEM_PROMPT), HumanMessage(state["x"])]}

    async def call_model(state: LlmState) -> dict[str, Any]:
        reply = await model.ainvoke(state["messages"])
        return {"text": reply.content}

    async def finish(state: LlmState) -> dict[str, Any]:
        return {"answer": state["text"]}

    builder = StateGraph(LlmState)
    builder.add_node("prepare", prepare)
    builder.add_node("llm", call_model)
    builder.add_node("finish", finish)
    builder.add_edge(START, "prepare")
    builder.add_edge("prepare", "llm")
    builder.add_edge("llm", "finish")
    builder.add_edge("finish", END)
    return builder.compile()


def langgraph_chain(length: int, use_async: bool):
    from langgraph.graph import END, START, StateGraph

    async def passthrough_async(state: ChainState) -> dict[str, Any]:
        return {"x": state["x"]}

    def passthrough_sync(state: ChainState) -> dict[str, Any]:
        return {"x": state["x"]}

    passthrough = passthrough_async if use_async else passthrough_sync
    builder = StateGraph(ChainState)
    previous = START
    for index in range(length):
        name = f"p{index}"
        builder.add_node(name, passthrough)
        builder.add_edge(previous, name)
        previous = name
    builder.add_edge(previous, END)
    return builder.compile()


def langgraph_fanout(width: int, use_async: bool):
    from langgraph.graph import END, START, StateGraph

    async def branch_async(state: FanoutState) -> dict[str, Any]:
        return {"outs": [state["x"]]}

    def branch_sync(state: FanoutState) -> dict[str, Any]:
        return {"outs": [state["x"]]}

    async def join_async(state: FanoutState) -> dict[str, Any]:
        return {"x": state["outs"][0]}

    def join_sync(state: FanoutState) -> dict[str, Any]:
        return {"x": state["outs"][0]}

    builder = StateGraph(FanoutState)
    names = [f"p{index}" for index in range(width)]
    for name in names:
        builder.add_node(name, branch_async if use_async else branch_sync)
        builder.add_edge(START, name)
    builder.add_node("join", join_async if use_async else join_sync)
    builder.add_edge(names, "join")  # waits for every branch
    builder.add_edge("join", END)
    return builder.compile()


def lcel_llm3():
    from langchain_core.output_parsers import StrOutputParser
    from langchain_core.prompts import ChatPromptTemplate

    prompt = ChatPromptTemplate.from_messages([("system", SYSTEM_PROMPT), ("human", "{x}")])
    return prompt | make_chat_model() | StrOutputParser()


def lcel_chain(length: int):
    from langchain_core.runnables import RunnableLambda, RunnableSequence

    async def passthrough(value):
        return value

    steps = [RunnableLambda(passthrough) for _ in range(length)]
    return RunnableSequence(*steps)


def lcel_fanout(width: int):
    from langchain_core.runnables import RunnableLambda, RunnableParallel

    async def passthrough(value):
        return value

    async def join(values):
        return values["p0"]

    branches = RunnableParallel({f"p{index}": RunnableLambda(passthrough) for index in range(width)})
    return branches | RunnableLambda(join)


def build(framework: str, shape: str):
    """Compile `shape` (llm3, chain-N, fanout-N) for `framework`."""
    kind, _, size_text = shape.partition("-")
    size = int(size_text) if size_text else 0
    if framework in ("langgraph", "langgraph-sync"):
        use_async = framework == "langgraph"
        if kind == "llm3":
            if not use_async:
                raise ValueError("llm3 is only built async (the idiomatic way to call a model)")
            return langgraph_llm3()
        if kind == "chain":
            return langgraph_chain(size, use_async)
        if kind == "fanout":
            return langgraph_fanout(size, use_async)
    if framework == "lcel":
        if kind == "llm3":
            return lcel_llm3()
        if kind == "chain":
            return lcel_chain(size)
        if kind == "fanout":
            return lcel_fanout(size)
    raise ValueError(f"unknown framework/shape: {framework} {shape}")
