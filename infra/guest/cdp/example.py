"""Example: search a page in the visible browser from Python.

python3 /usr/local/lib/elsebot/cdp/example.py URL QUERY
"""
import json
import sys

sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
import elsecdp  # noqa: E402

url, query = sys.argv[1:3]
with elsecdp.connect() as browser:
    page = browser.page(create=url)
    page.fill("input[name=q]", query)
    try:
        page.click("button[type=submit]")
    except elsecdp.Uncertain as e:
        print("the click may or may not have happened:", e, file=sys.stderr)
        raise
    page.wait_for("document.querySelector('#results')", timeout=10)
    print(json.dumps({"title": page.read("document.title"),
                      "results": page.read("document.querySelector('#results').innerText")}))
