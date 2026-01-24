FROM node:20-bookworm-slim

# Needed for native modules like sqlite3
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 make g++ \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app

RUN mkdir -p /data
ENV DB_PATH=/data/database.sqlite

# Install deps (devDeps included because gulp is in devDependencies)
COPY package*.json ./
RUN npm ci

# Copy source
COPY . .

RUN npm run build

EXPOSE 3000

CMD ["npm", "run", "start"]
