# syntax=docker/dockerfile:1
ARG NODE_IMAGE=node:24-alpine
ARG NGINX_IMAGE=nginxinc/nginx-unprivileged:1.28-alpine
FROM ${NODE_IMAGE} AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --ignore-scripts
COPY tsconfig.json vite.config.ts index.html ./
COPY src ./src
COPY public ./public
ARG VITE_APP_ORIGIN
ENV VITE_APP_ORIGIN=${VITE_APP_ORIGIN}
RUN npm run build

FROM ${NGINX_IMAGE}
COPY --from=build /app/dist /usr/share/nginx/html
COPY nginx.conf /etc/nginx/nginx.conf
COPY default.conf.template /etc/nginx/templates/default.conf.template
COPY --chmod=755 prepare-config.sh /docker-entrypoint.d/05-hibana-config.sh
ENV HIBANA_API_UPSTREAM=http://hibana-api:8080 NGINX_ENVSUBST_OUTPUT_DIR=/tmp/conf.d NGINX_ENVSUBST_FILTER=HIBANA_API_UPSTREAM
EXPOSE 8080
