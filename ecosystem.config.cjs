module.exports = {
  apps: [
    {
      name: 'rn-ota-server-rust',
      script: './target/release/rn-ota-server-rust',
      cwd: __dirname,
      instances: 1,
      exec_mode: 'fork',
      watch: false,
      max_memory_restart: '100M',
      env: {
        NODE_ENV: 'development',
        PORT: 3010,
        SERVICE_NAME: 'rn-ota-server-rust',
      },
      env_production: {
        NODE_ENV: 'production',
        PORT: 3010,
        SERVICE_NAME: 'rn-ota-server-rust',
      },
      log_date_format: 'YYYY-MM-DD HH:mm:ss',
      error_file: './logs/ota-server-error.log',
      out_file: './logs/ota-server-out.log',
      log_file: './logs/ota-server-combined.log',
      time: true,
      autorestart: true,
      max_restarts: 10,
      restart_delay: 4000,
      kill_timeout: 5000,
    },
  ],
};
