import rsloop
import aiohttp


async def fetch_url(url):
    async with aiohttp.ClientSession() as session:
        async with session.get(url) as response:
            return await response.text()


if __name__ == "__main__":
    html_content = rsloop.run(fetch_url("https://httpbin.org/get"))
    print(html_content)
