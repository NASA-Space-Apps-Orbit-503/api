# api-nasa

API HTTP en Rust para el calendario de actividad de quema (NASA Space Apps · *Harmonization of MODIS and VIIRS Hot Spots*). Guarda los hotspots de MODIS y VIIRS en una base **DuckDB**, los armoniza y calcula para cualquier área el calendario, la climatología mensual y las anomalías.

- Lenguaje: Rust + [axum](https://github.com/tokio-rs/axum) + [`duckdb-rs`](https://github.com/duckdb/duckdb-rs)
- Despliegue: Docker Compose (pensado para Dokploy)
- Contrato para el frontend: [`openapi.yaml`](./openapi.yaml) (OpenAPI 3.1)

---

## Índice

1. [Qué resuelve de cada parte del reto](#qué-resuelve-de-cada-parte-del-reto)
2. [Estructura del código](#estructura-del-código)
3. [Entidades](#entidades)
4. [Armonización MODIS / VIIRS](#armonización-modis--viirs)
5. [Configuración](#configuración)
6. [Despliegue en Dokploy](#despliegue-en-dokploy)
7. [Autenticación y CORS](#autenticación-y-cors)
8. [Endpoints](#endpoints)
9. [Filtros comunes](#filtros-comunes)
10. [Estadísticas para el calendario](#estadísticas-para-el-calendario)
11. [Cargar datos](#cargar-datos)
12. [Errores](#errores)
13. [Contrato para el frontend](#contrato-para-el-frontend)
14. [Límites y seguridad](#límites-y-seguridad)
15. [Desarrollo local](#desarrollo-local)
16. [Agregar una entidad nueva](#agregar-una-entidad-nueva)

---

## Qué resuelve de cada parte del reto

| Lo que pide el reto | Dónde |
|---|---|
| Armonizar MODIS y VIIRS en una serie consistente | `metric=cells` en todo `/stats/*` y vista `detections_harmonized` |
| Elegir un área (dibujarla o con latitud y longitud) | Filtros `bbox`, `lat`+`lon`+`radius_km` o `polygon` (GeoJSON) |
| Elegir un rango de fechas | Filtros `from` y `to` |
| Calendario de actividad desde los 2000 a hoy | `GET /stats/calendar` |
| Qué meses y años tienen más fuegos | `GET /stats/summary` y `GET /stats/climatology` |
| Detectar anomalías por celda y día frente a la historia | `GET /stats/anomalies` |
| Serie mensual MODIS → VIIRS armonizada (factor por mes, validación, anomalía) | `GET /stats/harmonized`, `GET /stats/harmonization` |
| Descargar un CSV de los hotspots de la búsqueda | `GET /detections?format=csv` |
| Datos de NASA FIRMS | `POST /detections/import`, `POST /firms/sync` y sincronización automática |

---

## Estructura del código

```
src/
├── main.rs                  # arranque, router y CORS
├── config.rs                # variables de entorno
├── state.rs                 # estado compartido y ejecución en DuckDB
├── db.rs                    # abre la base, aplica el esquema, extensiones y seguridad
├── schema.sql               # tablas, índices, vista armonizada e instrumentos base
├── auth.rs                  # header x-api-key (y lectura pública opcional)
├── error.rs                 # formato único de errores
├── validation.rs            # validaciones compartidas (fechas, horas, rangos…)
├── filters.rs               # filtros de área, tiempo, instrumento, satélite, confianza; filter[...]
├── analytics.rs             # /stats/*: calendario, climatología, resumen
├── anomalies.rs             # /stats/anomalies: anomalías por celda y día
├── harmonization.rs         # serie mensual armonizada (factor VIIRS → MODIS), recalculada al importar
├── firms.rs                 # lectura de CSV FIRMS, descarga y sincronización
├── explorer.rs              # /health, /tables, /query
└── entities/
    ├── mod.rs               # junta las rutas de todas las entidades
    ├── crud.rs              # crear (uno o varios), obtener por id, paginación (genérico)
    ├── instrument.rs        # Instrument
    ├── detection.rs         # Detection + búsqueda, CSV e importación
    └── burning_activity_stats.rs   # Burning Activity Stats + listado y recálculo
```

El esquema se crea solo al arrancar (`schema.sql` es idempotente). La primera vez también inserta los instrumentos **MODIS** y **VIIRS**.

---

## Entidades

### Instrument → tabla `instruments`

| Campo  | Tipo   | Obligatorio | Notas                                   |
|--------|--------|-------------|-----------------------------------------|
| `id`   | entero | (generado)  |                                         |
| `code` | texto  | Sí          | Único, máx. 20, se guarda en mayúsculas |
| `name` | texto  | Sí          | Máx. 200                                |

### Detection → tabla `detections`

| Campo        | Tipo   | Obligatorio | Notas                                                       |
|--------------|--------|-------------|-------------------------------------------------------------|
| `id`         | entero | (generado)  |                                                             |
| `latitude`   | número | Sí          | -90 a 90                                                    |
| `longitude`  | número | Sí          | -180 a 180                                                  |
| `bright_ti4` | número | No          | ≥ 0 (K). En MODIS es `brightness`                           |
| `scan`       | número | No          | ≥ 0                                                         |
| `track`      | número | No          | ≥ 0                                                         |
| `acq_date`   | texto  | Sí          | `YYYY-MM-DD`                                                |
| `acq_time`   | texto  | Sí          | Hora UTC; acepta `0412`, `412` o `04:12`, se guarda `0412`  |
| `satellite`  | texto  | Sí          | Máx. 20 (`N`, `N20`, `N21`, `Terra`, `Aqua`…)               |
| `instrument` | texto  | Sí          | Debe existir en `instruments.code`; se guarda en mayúsculas |
| `confidence` | texto  | No          | Se guarda como `low` / `nominal` / `high`. Acepta también VIIRS `l`/`n`/`h` y MODIS `0`–`100` (< 30 low, < 80 nominal, resto high) |
| `version`    | texto  | No          | Máx. 20 (`2.0NRT`)                                          |
| `bright_ti5` | número | No          | ≥ 0 (K). En MODIS es `bright_t31`                           |
| `frp`        | número | No          | ≥ 0 (MW)                                                    |
| `daynight`   | texto  | No          | `D` o `N`                                                   |

Una detección es única por **lugar + fecha + hora + satélite + instrumento**: importar el mismo CSV dos veces no duplica datos.

### Burning Activity Stats → tabla `burning_activity_stats`

| Campo     | Tipo   | Obligatorio | Notas                         |
|-----------|--------|-------------|-------------------------------|
| `id`      | entero | (generado)  |                               |
| `day`     | entero | Sí          | Debe existir en ese mes y año |
| `month`   | entero | Sí          | 1 a 12                        |
| `year`    | entero | Sí          | 1900 a 2100                   |
| `count`   | entero | Sí          | ≥ 0                           |
| `max_frp` | número | No          | ≥ 0 (MW)                      |

La tabla guarda **una** serie diaria (un área). Se llena a mano o con `POST /burning-activity-stats/rebuild`. Para áreas que elige el usuario usa `/stats/*`, que calcula en el momento.

Los campos desconocidos se rechazan con `400`.

---

## Armonización MODIS / VIIRS

MODIS (Terra y Aqua, desde 2000) y VIIRS (S-NPP desde 2012, NOAA-20 y NOAA-21) no se pueden comparar directamente:

| Diferencia | Cómo se resuelve |
|---|---|
| **Resolución:** un fuego que MODIS ve como 1 píxel de 1 km, VIIRS lo ve como varios de 375 m | `metric=cells`: se cuentan **celdas de una cuadrícula común** con al menos un hotspot por día, no puntos. La celda por defecto es `cell_deg=0.01` (≈ 1.1 km, parecida al píxel de MODIS) |
| **Varios satélites ven el mismo fuego** | La misma celda el mismo día cuenta una sola vez, sin importar cuántos satélites la detectaron |
| **Columnas distintas** (`brightness`/`bright_t31` vs `bright_ti4`/`bright_ti5`) | Al importar, MODIS `brightness` → `bright_ti4` y `bright_t31` → `bright_ti5` |
| **Confianza en escalas distintas** (MODIS 0–100, VIIRS `l`/`n`/`h`) | Al guardar, `confidence` se convierte a `low` / `nominal` / `high` (MODIS: < 30, < 80, resto). La vista `detections_harmonized` agrega `confidence_level` (1, 2, 3) para el filtro `min_confidence` |

`metric=detections` da los conteos crudos, para comparar.

Para la serie más consistente desde 2000, se recomienda combinar MODIS (todo el periodo) con VIIRS (desde 2012) usando `metric=cells`. Si quieres revisar el efecto de cada sensor, filtra con `instrument=MODIS` o `instrument=VIIRS` y compara en el periodo en que ambos coinciden.

### Serie armonizada mensual (factor VIIRS → MODIS)

Cada importación que guarda filas nuevas (`POST /detections/import`, `POST /firms/sync` y la sincronización automática) recalcula sobre **todo el histórico** y guarda el resultado en `harmonized_monthly`, `harmonization_factors`, `harmonization_validation`, `harmonization_static_cells` y `harmonization_runs`. No se calcula por CSV porque el factor necesita los meses en que ambos sensores tienen datos.

1. **Unificar:** solo MODIS y VIIRS con satélite reconocido (`satellite_name` de la vista: `N`, `1`, `J1`… → `Suomi NPP`, `NOAA-20`…); sin duplicados por satélite, instante y coordenadas a 4 decimales; dentro de `HARMONIZE_BBOX`; fecha local con `HARMONIZE_UTC_OFFSET`; celda de `HARMONIZE_CELL_DEG`.
2. **Limpiar:** confianza mínima, `type = 0` (sin volcanes, fuentes estáticas ni mar) y fuera las celdas con fuego en más del `HARMONIZE_STATIC_THRESHOLD` de los meses. Este último filtro solo se aplica con al menos `HARMONIZE_STATIC_MIN_MONTHS` meses de datos: con pocos meses, toda celda con fuego parecería estática.
3. **Satélites:** MODIS solo Aqua (pasa ~13:30, igual que VIIRS); VIIRS un satélite por día según `HARMONIZE_VIIRS_PRIORITY` (Suomi NPP hasta `HARMONIZE_SNPP_END`, luego NOAA-20).
4. **Celdas-día:** celdas con fuego por día y sensor (días sin fuego = 0), sumadas por mes.
5. **Factor por mes calendario** = Σ MODIS / Σ VIIRS en los meses completos de ambos sensores hasta `HARMONIZE_OVERLAP_END` (razón de sumas). Si VIIRS tiene menos de `HARMONIZE_MIN_CELLS_MONTH` celdas-día en ese mes, se usa el factor de su época (seca = `HARMONIZE_DRY_MONTHS`, lluviosa = el resto).
6. **Serie:** MODIS Aqua antes del traslape; VIIRS × factor (equivalente MODIS) desde el primer mes de traslape, así sigue cuando MODIS se apague.
7. **Validación** dejando fuera un año a la vez y **anomalía robusta** `(valor − mediana) / (p75 − p25)` del mismo mes en `HARMONIZE_REFERENCE_YEARS`: ≤ −1 `very_low` · ≤ −0.5 `low` · ≤ 0.5 `normal` · ≤ 1 `high` · > 1 `very_high`.

`status` de la corrida: `ok`, `no_overlap` (no hay meses completos de ambos sensores; la serie es solo MODIS) o `insufficient_data` (no hay datos de Aqua MODIS). Si el traslape tiene un solo año, no hay validación por año (no queda con qué calcular el factor sin ese año).

La sincronización automática recalcula una sola vez por ciclo, al final de todas las fuentes. `POST /detections` (crear a mano) **no** recalcula. Tras eso, tras cambiar `HARMONIZE_*` o tras cargar con el CLI de DuckDB, usa `POST /harmonization/rebuild`.

---

## Configuración

| Variable             | Por defecto                       | Descripción |
|----------------------|-----------------------------------|-------------|
| `API_KEY`            | — (obligatoria)                   | Clave de la API |
| `DB_PATH`            | `/data/marro.duckdb`              | Archivo DuckDB dentro del contenedor |
| `PORT`               | `8080`                            | Puerto HTTP |
| `CORS_ORIGINS`       | `*`                               | Orígenes del frontend separados por coma (`https://mi-front.com,http://localhost:5173`) o `*` |
| `PUBLIC_READ`        | `false`                           | `true` = los `GET` no piden API key |
| `MAX_ROWS`           | `10000`                           | Máximo de filas en respuestas JSON |
| `MAX_EXPORT_ROWS`    | `200000`                          | Máximo de filas por descarga CSV |
| `MAX_BULK`           | `10000`                           | Máximo de elementos al crear con un arreglo |
| `MAX_BODY_MB`        | `50`                              | Tamaño máximo del cuerpo de una petición |
| `FIRMS_MAP_KEY`      | —                                 | MAP_KEY de NASA FIRMS ([pídela aquí](https://firms.modaps.eosdis.nasa.gov/api/map_key/)) |
| `FIRMS_SYNC_HOURS`   | `0`                               | Cada cuántas horas sincronizar (0 = no sincroniza solo) |
| `FIRMS_SYNC_AREA`    | `-90.13,13.15,-87.68,14.45`       | Área a sincronizar (por defecto El Salvador) |
| `FIRMS_SYNC_SOURCES` | `VIIRS_SNPP_NRT,VIIRS_NOAA20_NRT,VIIRS_NOAA21_NRT,MODIS_NRT` | Fuentes a sincronizar |
| `FIRMS_SYNC_DAYS`    | `2`                               | Días hacia atrás en cada sincronización (1–10) |
| `HARMONIZE_BBOX`     | el de `FIRMS_SYNC_AREA`           | Área de la serie armonizada (`none` = sin recorte). Conviene que coincida con el área de sincronización, para que los meses NRT y los del archivo cubran lo mismo |
| `HARMONIZE_CELL_DEG` | `0.01`                            | Celda en grados (≈ 1.1 km) |
| `HARMONIZE_MIN_CONFIDENCE` | `nominal`                   | `nominal` (nominal + high) o `high` |
| `HARMONIZE_STATIC_THRESHOLD` | `0.30`                    | Fracción de meses con fuego para marcar una celda como fuente estática |
| `HARMONIZE_STATIC_MIN_MONTHS` | `12`                     | Meses de datos mínimos para aplicar el filtro de fuentes estáticas |
| `HARMONIZE_SNPP_END` | `2026-11-02`                      | Fin de Suomi NPP (desde ese día se usa el siguiente VIIRS) |
| `HARMONIZE_VIIRS_PRIORITY` | `Suomi NPP,NOAA-20,NOAA-21` | Satélite VIIRS de referencia, en orden |
| `HARMONIZE_OVERLAP_END` | `2025-12`                      | Último mes (YYYY-MM) usado para calcular el factor |
| `HARMONIZE_MIN_CELLS_MONTH` | `30`                       | Menos celdas-día VIIRS en un mes → factor de la época |
| `HARMONIZE_DRY_MONTHS` | `11,12,1,2,3,4`                 | Meses de la época seca |
| `HARMONIZE_REFERENCE_YEARS` | `2003-2025`                | Años para la mediana de la anomalía |
| `HARMONIZE_UTC_OFFSET` | `-6`                            | Horas de diferencia con UTC para la fecha local |

Genera la API key con `openssl rand -hex 32`. Si tu base se llama distinto, cambia `DB_PATH` (por ejemplo `/data/nasa.duckdb`).

---

## Despliegue en Dokploy

1. Sube este repositorio a GitHub.
2. En Dokploy: **Create Service → Compose → Git**. Compose path: `./docker-compose.yml`.
3. En **Environment**, como mínimo:
   ```
   API_KEY=tu_clave_generada
   CORS_ORIGINS=https://tu-frontend.com,http://localhost:5173
   PUBLIC_READ=true
   FIRMS_MAP_KEY=tu_map_key
   FIRMS_SYNC_HOURS=6
   ```
4. En **Domains**: dominio para el servicio `api`, puerto `8080`, con HTTPS.
5. **Deploy**.

```bash
curl https://api.tudominio.com/health
# {"ok":true}
```

> El primer build tarda varios minutos y necesita unos 4 GB de RAM porque compila DuckDB. Si Dokploy lo mata por memoria (`killed`, `signal 9`), agrega swap al servidor o construye la imagen fuera (laptop o GitHub Actions) y despliégala como imagen.
>
> Al arrancar, la API descarga las extensiones `json` y `spatial` de DuckDB (necesita salida a internet). Si `spatial` no se puede cargar, todo funciona menos el filtro por polígono.

---

## Autenticación y CORS

- Escritura (`POST`) y `/query`: siempre con header `x-api-key`.
- Lectura (`GET`): con `x-api-key`, o sin ella si `PUBLIC_READ=true`.
- `CORS_ORIGINS` define qué frontends pueden llamar a la API desde el navegador.

Con `PUBLIC_READ=true` el frontend no necesita guardar la API key: todo lo que muestra (búsquedas, CSV, calendario, estadísticas) es `GET`. La carga de datos se hace desde un script o desde el panel de administración.

En los ejemplos:

```bash
export API=https://api.tudominio.com
export KEY=tu_clave_generada
export SV="bbox=-90.13,13.15,-87.68,14.45"     # El Salvador
```

---

## Endpoints

### Entidades

| Método | Ruta                               | Descripción                                     |
|--------|------------------------------------|-------------------------------------------------|
| GET    | `/instruments`                     | Listar instrumentos                             |
| POST   | `/instruments`                     | Crear uno (objeto) o varios (arreglo)           |
| GET    | `/instruments/{id}`                | Obtener uno                                     |
| GET    | `/detections`                      | Buscar con filtros (JSON paginado o **CSV**)    |
| POST   | `/detections`                      | Crear una (objeto) o varias (arreglo)           |
| POST   | `/detections/import`               | Importar un CSV de FIRMS                        |
| GET    | `/detections/{id}`                 | Obtener una                                     |
| GET    | `/burning-activity-stats`          | Listar (`?year=&month=`)                        |
| POST   | `/burning-activity-stats`          | Crear una (objeto) o varias (arreglo)           |
| POST   | `/burning-activity-stats/rebuild`  | Recalcular desde las detecciones                |
| GET    | `/burning-activity-stats/{id}`     | Obtener una                                     |

### Estadísticas, FIRMS y exploración

| Método | Ruta                  | Descripción                                          |
|--------|-----------------------|------------------------------------------------------|
| GET    | `/stats/calendar`     | Serie por mes o por día (con ceros)                  |
| GET    | `/stats/climatology`  | Mediana, promedio, etc. de cada mes entre años       |
| GET    | `/stats/anomalies`    | Anomalías por celda y día (paginado, ordenable)      |
| GET    | `/stats/anomalies/{id}` | Una anomalía por id                                |
| GET    | `/stats/summary`      | Totales, años y meses con más actividad              |
| GET    | `/stats/harmonized`   | Serie mensual armonizada con anomalía (`year`, `month`, `from`, `to`, `complete`) |
| GET    | `/stats/harmonization` | Última corrida, factores por mes y validación por año |
| POST   | `/harmonization/rebuild` | Recalcular la armonización                        |
| POST   | `/firms/sync`         | Descargar de FIRMS e importar                        |
| GET    | `/health`             | Estado del servicio (siempre público)                |
| GET    | `/tables`             | Tablas y vistas                                      |
| GET    | `/tables/{nombre}`    | Primeras filas                                       |
| POST   | `/query`              | SQL de solo lectura                                  |

### Crear: un objeto o un arreglo en la misma ruta

Los `POST` de las tres entidades aceptan las dos formas:

| Cuerpo | Respuesta `201` |
|---|---|
| Un objeto `{...}` | El registro creado, con su `id` |
| Un arreglo `[{...}, {...}]` | `{"count": n, "items": [...]}` |

Con un arreglo es **todo o nada**: si un elemento falla no se guarda ninguno, y el error trae `index` con la posición (base 0) del que falló.

### Ejemplos de las entidades

```bash
# Crear una detección
curl -X POST "$API/detections" -H "x-api-key: $KEY" -H "Content-Type: application/json" \
  -d '{"latitude":13.6929,"longitude":-89.2182,"acq_date":"2024-03-14","acq_time":"0748",
       "satellite":"N","instrument":"VIIRS","confidence":"nominal","frp":4.2,"daynight":"N"}'

# Crear varias estadísticas: misma ruta, el cuerpo es un arreglo
curl -X POST "$API/burning-activity-stats" -H "x-api-key: $KEY" -H "Content-Type: application/json" \
  -d '[{"day":14,"month":3,"year":2024,"count":57,"max_frp":38.4},
       {"day":15,"month":3,"year":2024,"count":12}]'
# → 201 {"count":2,"items":[{"id":1,...},{"id":2,...}]}

# Obtener por id
curl -H "x-api-key: $KEY" "$API/instruments/1"
```

### Buscar y descargar detecciones

```bash
# JSON paginado: {"total": 1234, "limit": 100, "offset": 0, "items": [...]}
curl -H "x-api-key: $KEY" "$API/detections?$SV&from=2024-01-01&to=2024-04-30&min_confidence=nominal&limit=100"

# CSV descargable con todos los hotspots de la búsqueda
curl -H "x-api-key: $KEY" -OJ "$API/detections?$SV&from=2024-01-01&to=2024-04-30&format=csv"
```

---

## Filtros comunes

Los aceptan `GET /detections`, todo `/stats/*` y `POST /burning-activity-stats/rebuild`.

| Parámetro        | Ejemplo                               | Descripción |
|------------------|---------------------------------------|-------------|
| `bbox`           | `-90.13,13.15,-87.68,14.45`           | Rectángulo oeste,sur,este,norte |
| `lat`, `lon`, `radius_km` | `lat=13.7&lon=-89.2&radius_km=25` | Círculo alrededor de un punto |
| `polygon`        | `{"type":"Polygon","coordinates":[...]}` (codificado en la URL) | Área dibujada en el mapa (GeoJSON) |
| `from`, `to`     | `2001-01-01`, `2024-12-31`            | Rango de fechas inclusivo |
| `date`           | `2024-03-14`                          | Un día exacto |
| `year`           | `2023,2024`                           | Uno o varios años |
| `month`          | `3,4`                                 | Uno o varios meses (1..12) |
| `day`            | `1,15`                                | Uno o varios días del mes (1..31) |
| `instrument`     | `MODIS,VIIRS`                         | Uno o varios instrumentos |
| `satellite`      | `N,N20,Terra`                         | Uno o varios satélites |
| `min_confidence` | `low` \| `nominal` \| `high`          | Confianza mínima armonizada |
| `daynight`       | `D` \| `N`                            | Solo de día o de noche |

El área se indica de **una sola** forma. Los filtros se combinan con AND. `GET /burning-activity-stats` acepta `year`, `month` y `day`.

**Sintaxis `filter[...]`** (estilo spatie/laravel-query-builder): cada filtro también se puede mandar como `filter[nombre]=valor`. Un `filter[...]` desconocido responde `400` con la lista de permitidos, en vez de ignorarse.

```bash
curl -H "x-api-key: $KEY" "$API/detections?filter[bbox]=-90.13,13.15,-87.68,14.45&filter[month]=3&filter[day]=14"
```

Para las estadísticas, además:

| Parámetro  | Por defecto | Descripción |
|------------|-------------|-------------|
| `metric`   | `cells`     | `cells` (armonizado) o `detections` (crudo) |
| `cell_deg` | `0.01`      | Tamaño de celda en grados (0.001 a 1) |

Desde JavaScript, un polígono dibujado (por ejemplo con Leaflet o MapLibre) se manda así:

```ts
const params = new URLSearchParams({
  polygon: JSON.stringify(feature.geometry),  // { type: "Polygon", coordinates: [...] }
  from: "2001-01-01",
});
fetch(`${API}/stats/calendar?${params}`);
```

---

## Estadísticas para el calendario

Todas se calculan en el momento para el área y filtros que pidas.

### Calendario: `GET /stats/calendar`

Una fila por mes (o por día con `granularity=day`), **incluidos los meses sin actividad con `count: 0`**. Es exactamente la matriz años × meses que se pinta con la rampa de colores.

```bash
curl -H "x-api-key: $KEY" "$API/stats/calendar?$SV&from=2001-01-01"
```

```json
{
  "metric": "cells", "cell_deg": 0.01, "granularity": "month",
  "from": "2001-01-01", "to": null,
  "items": [
    { "year": 2001, "month": 1, "day": null, "count": 12, "max_frp": 45.1, "active_days": 6 },
    { "year": 2001, "month": 2, "day": null, "count": 87, "max_frp": 120.4, "active_days": 19 }
  ]
}
```

### Climatología: `GET /stats/climatology`

Para cada mes del año (1–12): `median`, `mean`, `stddev`, `min`, `max`, `p25`, `p75`, cuántos años entran (`years`) y qué año tuvo el máximo (`max_year`). Sirve para la línea de "normal histórico" y para escalar la rampa de colores.

### Anomalías: `GET /stats/anomalies`

Divide el área en celdas de `cell_deg` grados (por defecto `0.1` ≈ 11 km) y, para cada celda y día con actividad, compara las detecciones **observadas** con las **esperadas**: el promedio diario de esa celda en el mismo mes de los años anteriores (desde el primer año con datos del área).

- `score = 2 · (√(observed + 3/8) − √(expected + 3/8))`: desviación de Poisson con la transformación de Anscombe. Se lee como un z-score y está definida aunque lo esperado sea 0.
- `severity`: `low` ≥ 2, `moderate` ≥ 3, `high` ≥ 5, `extreme` ≥ 8 (por debajo, `normal`).
- Los filtros de tiempo eligen **qué días** se reportan; la referencia siempre usa todos los años anteriores.

| Parámetro            | Por defecto | Descripción |
|----------------------|-------------|-------------|
| `cell_deg`           | `0.1`       | Tamaño de celda (0.001 a 1) |
| `min_baseline_years` | `3`         | Años anteriores mínimos para tener referencia |
| `min_score`          | `2`         | Score mínimo |
| `severity`           | —           | `low,moderate,high,extreme` (uno o varios) |
| `sort`               | `-score`    | `score`, `-score`, `date`, `-date`, `observed`, `-observed` |
| `limit`, `offset`    | `100`, `0`  | Paginación |

```bash
curl -H "x-api-key: $KEY" "$API/stats/anomalies?$SV&year=2024&month=3&severity=high,extreme"
```

```json
{
  "cell_deg": 0.1, "total": 42, "limit": 100, "offset": 0,
  "items": [
    { "id": "2024-03-14_0.1_136_-893", "date": "2024-03-14",
      "location": { "latitude": 13.65, "longitude": -89.25, "cell_deg": 0.1 },
      "observed": 37, "expected": 1.8, "ratio": 20.6, "score": 9.28, "severity": "extreme",
      "max_frp": 88.1, "baseline_years": 23 }
  ]
}
```

`GET /stats/anomalies/{id}` devuelve una sola. El id codifica fecha y celda; se recalcula con los filtros de área y atributos de la query (usa los mismos que en el listado para obtener el mismo valor).

> Para comparar sensores con resoluciones distintas, filtra por `instrument` (por ejemplo `instrument=MODIS`): VIIRS existe desde 2012 y detecta más píxeles por incendio.

### Resumen: `GET /stats/summary`

Detecciones crudas, total según la métrica, primera y última fecha, FRP máximo, detecciones por instrumento, y los años y meses con más actividad (`top`, por defecto 5).

---

## Cargar datos

### 1. Sincronización automática con FIRMS (datos recientes)

Con `FIRMS_MAP_KEY` y `FIRMS_SYNC_HOURS=6`, la API descarga cada 6 horas los últimos `FIRMS_SYNC_DAYS` días de cada fuente en `FIRMS_SYNC_AREA` y los importa sin duplicar.

### 2. Sincronización manual: `POST /firms/sync`

```bash
curl -X POST "$API/firms/sync" -H "x-api-key: $KEY" -H "Content-Type: application/json" \
  -d '{"source":"VIIRS_SNPP_SP","area":"-90.13,13.15,-87.68,14.45","days":10,"date":"2023-03-01"}'
```

```json
{ "source": "VIIRS_SNPP_SP", "area": "-90.13,13.15,-87.68,14.45", "days": 10, "date": "2023-03-01",
  "received": 845, "valid": 845, "inserted": 845, "duplicates": 0, "invalid": 0, "errors": [] }
```

Fuentes: `MODIS_NRT`, `MODIS_SP`, `VIIRS_SNPP_NRT`, `VIIRS_SNPP_SP`, `VIIRS_NOAA20_NRT`, `VIIRS_NOAA20_SP`, `VIIRS_NOAA21_NRT`. Las `_SP` (procesamiento estándar) son el archivo histórico; las `_NRT`, los datos casi en tiempo real. FIRMS limita cada petición a pocos días y a un número de transacciones por cada 10 minutos.

### 3. Importar un CSV: `POST /detections/import`

Para archivos descargados de [FIRMS Archive Download](https://firms.modaps.eosdis.nasa.gov/download/) (el histórico completo de un país o área, MODIS desde 2000 y VIIRS desde 2012):

```bash
curl -X POST "$API/detections/import" -H "x-api-key: $KEY" \
  -H "Content-Type: text/csv" --data-binary @fire_archive_M-C61_123456.csv
```

- Funciona con CSV de MODIS y de VIIRS, columnas en cualquier orden.
- Si alguna fila es inválida, por defecto **no guarda nada** y responde `422` con la lista de errores (`line` y `error`). Con `?skip_invalid=true` guarda las válidas.
- Las detecciones que ya existen se cuentan como `duplicates`.
- Se lee la columna `type` del archivo SP (0 vegetación, 1 volcán, 2 fuente estática, 3 mar). Si la detección ya existía sin `type` (vino de un CSV NRT), se completa y se cuenta en `updated`.
- Si hubo filas nuevas o completadas, recalcula la serie armonizada y devuelve su resumen en `harmonization` (o `harmonization_error`; las filas igual quedan guardadas). Para cargar muchos CSV seguidos, usa `?harmonize=false` y al final `POST /harmonization/rebuild`.
- Para archivos de muchos MB, divídelos en partes o usa el CLI (siguiente punto).

### 4. Cargas muy grandes con el CLI de DuckDB

```bash
docker compose stop api
docker run --rm -it -v <proyecto>_duckdb_data:/data duckdb/duckdb:2.0.0 duckdb /data/marro.duckdb
```

> **Usa siempre un CLI de la misma versión que la API (DuckDB 2.0.0).** Si abres el archivo con una versión más nueva, la API puede no leerlo y al arrancar falla con `Trying to read a database file with version number ...`.

```sql
-- VIIRS
INSERT OR IGNORE INTO detections (latitude, longitude, bright_ti4, scan, track, acq_date, acq_time,
                                  satellite, instrument, confidence, "version", bright_ti5, frp, daynight)
SELECT latitude, longitude, bright_ti4, scan, track, acq_date, lpad(acq_time::VARCHAR, 4, '0'),
       satellite::VARCHAR, 'VIIRS', confidence::VARCHAR, "version"::VARCHAR, bright_ti5, frp, daynight
FROM read_csv('/data/fire_archive_SV-C2.csv');

-- MODIS (columnas armonizadas)
INSERT OR IGNORE INTO detections (latitude, longitude, bright_ti4, scan, track, acq_date, acq_time,
                                  satellite, instrument, confidence, "version", bright_ti5, frp, daynight)
SELECT latitude, longitude, brightness, scan, track, acq_date, lpad(acq_time::VARCHAR, 4, '0'),
       satellite::VARCHAR, 'MODIS', confidence::VARCHAR, "version"::VARCHAR, bright_t31, frp, daynight
FROM read_csv('/data/fire_archive_M-C61.csv');
.exit
```

```bash
docker compose start api
```

DuckDB solo permite un proceso con escritura a la vez: por eso hay que detener la API antes. Al volver a arrancar, la API convierte `confidence` a `low` / `nominal` / `high`; después recalcula la serie con `POST /harmonization/rebuild`.

---

## Errores

```json
{ "error": "descripción del problema" }
{ "error": "latitude debe estar entre -90 y 90", "index": 3 }   // al crear con un arreglo
```

| Código | Cuándo |
|--------|--------|
| `400`  | JSON mal formado, tipo incorrecto, campo desconocido, filtro inválido o `filter[...]` desconocido, SQL inválido |
| `401`  | Falta `x-api-key` o es incorrecta |
| `404`  | No existe un registro con ese id |
| `409`  | Duplicado (instrumento o detección) o `instrument` inexistente |
| `413`  | Arreglo con más elementos que `MAX_BULK`, o cuerpo mayor que `MAX_BODY_MB` |
| `422`  | Valor fuera de las reglas; CSV con filas inválidas (con lista de errores) |
| `501`  | Se pidió `polygon` y el servidor no tiene la extensión spatial |
| `502`  | FIRMS respondió con error (MAP_KEY inválida, límite de uso…) |
| `503`  | Falta `FIRMS_MAP_KEY`, o `/health` no llega a la base |

---

## Contrato para el frontend

[`openapi.yaml`](./openapi.yaml) describe todos los endpoints, parámetros, cuerpos, respuestas y errores.

**Documentación interactiva:** abre el archivo en [editor.swagger.io](https://editor.swagger.io) o con la extensión *OpenAPI (Swagger) Editor* de VS Code.

**Tipos de TypeScript y cliente tipado** ([`openapi-typescript`](https://openapi-ts.dev) + [`openapi-fetch`](https://openapi-ts.dev/openapi-fetch/)):

```bash
npm i openapi-fetch
npx openapi-typescript ./openapi.yaml -o src/api/schema.d.ts
```

```ts
import createClient from "openapi-fetch";
import type { paths, components } from "./api/schema";

type CalendarPoint = components["schemas"]["CalendarPoint"];

const api = createClient<paths>({ baseUrl: import.meta.env.VITE_API_URL });

// Calendario de El Salvador desde 2001
const { data: calendar } = await api.GET("/stats/calendar", {
  params: { query: { bbox: "-90.13,13.15,-87.68,14.45", from: "2001-01-01" } },
});

// Anomalías graves de marzo de 2024
const { data: anomalies } = await api.GET("/stats/anomalies", {
  params: { query: { bbox: "-90.13,13.15,-87.68,14.45", year: "2024", month: "3", severity: "high,extreme" } },
});

// Enlace de descarga del CSV de la búsqueda actual
const csvUrl = `${import.meta.env.VITE_API_URL}/detections?` +
  new URLSearchParams({ bbox: "-90.13,13.15,-87.68,14.45", from: "2024-01-01", format: "csv" });
```

Con `PUBLIC_READ=true` no hace falta mandar la API key desde el navegador. Si cambias algo en Rust, actualiza `openapi.yaml` y vuelve a generar los tipos.

---

## Límites y seguridad

- **Escritura solo por los endpoints de entidades, importación y FIRMS**, todos con API key y validación.
- **`/query` es de solo lectura** y siempre pide API key.
- **Sin acceso a archivos desde SQL:** se desactiva el acceso externo de DuckDB tras cargar las extensiones.
- **Máximos** de filas por respuesta (`MAX_ROWS`), por CSV (`MAX_EXPORT_ROWS`), por arreglo en un `POST` (`MAX_BULK`) y de cuerpo (`MAX_BODY_MB`).
- **HTTPS:** expón la API solo por el dominio con HTTPS de Dokploy.

---

## Desarrollo local

Requisitos: Rust estable y un compilador de C++ (`g++` o `clang`).

```bash
export API_KEY=dev DB_PATH=./marro.duckdb PUBLIC_READ=true
cargo run --release      # la primera vez tarda: compila DuckDB
cargo test               # pruebas de validación, filtros, CSV de FIRMS y estadísticas
```

Con Docker: `API_KEY=dev docker compose up --build`, agregando temporalmente `ports: ["8080:8080"]` al servicio `api`.

---

## Agregar una entidad nueva

1. Agrega la tabla y su secuencia en `src/schema.sql`.
2. Crea `src/entities/<entidad>.rs` copiando uno existente: struct de la entidad, struct `New…`, `impl Entity` (SQL, validación, `from_row`) y `routes()`.
3. Regístrala en `src/entities/mod.rs` (`pub mod` y `.merge(...)`).
4. Documenta los endpoints y esquemas en `openapi.yaml`.
