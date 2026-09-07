export default {
  async fetch(request: Request, env: { GREETING: string }): Promise<Response> {
    const path = new URL(request.url).pathname;
    if (path === "/echo" && request.method === "POST") {
      return new Response(await request.arrayBuffer());
    }
    if (path === "/") return Response.json({ message: env.GREETING });
    return new Response("Not found", { status: 404 });
  },
};
