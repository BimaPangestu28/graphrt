"""Fake order-desk tools for examples/evals/agent: deterministic answers, no network."""

import os
import sys

try:
    from graphrt_plugin import Plugin, PluginError
except ImportError:  # running from the repository checkout
    sys.path.insert(
        0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "..", "..", "sdk", "python")
    )
    from graphrt_plugin import Plugin, PluginError

ORDERS = {
    "123": {"status": "shipped", "delivery": "Friday"},
    "456": {"status": "delivered", "delivery": "last Monday"},
}

plugin = Plugin("orders")

ORDER_ID = {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]}


def order(args):
    order_id = str(args.get("order_id", "")).strip()
    if order_id not in ORDERS:
        raise PluginError(f"unknown order {order_id!r}", kind="not_found")
    return order_id, ORDERS[order_id]


@plugin.tool("search_orders", description="Look up an order by id: status and delivery date", parameters=ORDER_ID)
async def search_orders(args):
    order_id, found = order(args)
    return {"text": f"order {order_id}: {found['status']}, delivery {found['delivery']}", "data": found}


@plugin.tool("refund_order", description="Refund an order by id", parameters=ORDER_ID)
async def refund_order(args):
    order_id, _ = order(args)
    return {"text": f"order {order_id} refunded", "data": {"refunded": True}}


if __name__ == "__main__":
    plugin.run()
